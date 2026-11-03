# Release procedure and ship gate

English | [日本語](RELEASE.md)

This turns spec 9.5 (SBOM, dependency licenses, signed releases, reproducible builds), 15.2 (AC-01, AC-12, AC-21, AC-25),
15.3 (security ship gate), REQ-007 AC-R7-03 and REQ-010 R10-6 into procedures and tools that can be run by hand.
The tools are in `tools/fuzz/`, `tools/release/`, `tools/loadgen/` and `tools/soak/`. Results go to `out/` (not tracked by git).

## Ship gate checklist

For each release candidate (pinned to a single commit), nothing is published until every item is met. Results, logs and reproduction
steps are kept as release evidence (spec 15.3).

| # | Item | Tool / procedure | Pass condition |
|---|---|---|---|
| G1 | Unit and integration tests | `cargo test --workspace`, `cargo clippy --workspace --all-targets -- -D warnings` | Everything passes |
| G2 | Interoperability | `tools/interop/*.sh` (each row of the conformance ledger `docs/dev/conformance/ledger.md`) | No row of the ledger has regressed since the previous release |
| G3 | Fuzzing (AC-01, 15.3) | `tools/fuzz/all.sh 86400` (24 hours or more per target) | No crash, timeout or OOM on any target. Record the corpus, edges reached, unreached areas, findings and fixes |
| G4 | Sanitizers (FFI) | G3 with `FUZZ_SANITIZER=address` (the default). For codec workers that include FFI, see "Sanitizers" below | No AddressSanitizer reports |
| G5 | Load (AC-12, R10-6) | `tools/loadgen/load.sh` (in the reference environment of spec 13.1, with senders and viewers on separate machines) | The capacity conditions of chapter 13. Report losses and delays |
| G6 | Long run (AC-21) | `tools/soak/soak.sh 259200` (72 hours, with churn) | Zero crashes, RSS growth after warm-up within 5%, fds and sockets reclaimed |
| G7 | SBOM and licenses (AC-25, AC-R7-03) | `tools/release/sbom.sh`, `tools/release/license_check.py` (run automatically inside G9) | No disallowed licenses or forbidden components. REVIEW items have a recorded check |
| G8 | Independent review (15.3) | Cryptographic protocols (TLS, DTLS, SRTP, SRT encryption, QUIC), authorization (JWT, signed URLs, tokens), distributed ownership (fencing, mTLS) | A review report by someone independent of the implementer. No unresolved Critical/High findings. For the rest, record impact, mitigation, owner and deadline |
| G9 | Reproducible build and signing | `REPRO_CHECK=1 SIGNING_KEY=… tools/release/build.sh` | Two builds are byte-identical, and the signature on SHA256SUMS verifies |
| G10 | Vulnerability monitoring | RustSec (`cargo audit` and so on) against the SBOM | Handling of known vulnerabilities is recorded |

The per-feature status in `docs/dev/conformance/ledger.md` (experimental / preview / production, spec 15.4) is decided by the results of
this gate. Performance figures that have not been met are not published as guarantees.

## Fuzzing (G3, G4)

Coverage-guided fuzzing (cargo-fuzz / libFuzzer). The crate is `fuzz/` (outside the workspace; it needs nightly).

```sh
rustup toolchain install nightly --profile minimal
cargo install cargo-fuzz
tools/fuzz/seed.sh                 # create seeds in fuzz/corpus/<target>/ (test data, FFmpeg output, in-house round trips, hand-written)
tools/fuzz/run.sh rtmp_server 600  # one target for 10 minutes
tools/fuzz/all.sh 60               # every target for 60 seconds (CI smoke)
PARALLEL=8 tools/fuzz/all.sh 86400 # release gate: 24 hours per target
```

- Targets (28): `amf0` `flv` `rtmp_server` `mpegts` `fmp4` `hls` `http` `sdp` `stun` `rtp` `rtcp` `rtp_depack`
  `rtsp_client` `rtsp_message` `srt_listener` `srt_packet` `dtls_server` `h264` `h265` `aac` `av1` `opus` `record`
  `jwt` `png` `jpeg` `raft` `tls_server`. The input format (leading selector byte, datagram framing) is described at the top of each `fuzz/fuzz_targets/*.rs`.
- Limits: `-rss_limit_mb=2048` and 10 seconds per input (change with `FUZZ_RSS_MB` and `FUZZ_TIMEOUT`).
- Results: `fuzz/artifacts/summary.md` (per target: runs, exec/s, cov (edges), ft, corpus, peak RSS, reproducers),
  logs in `fuzz/artifacts/logs/<target>.log`, reproducers in `fuzz/artifacts/<target>/crash-*` (`oom-*`, `timeout-*`).
- Reproducing: `RUST_BACKTRACE=1 ~/.cache/senca-fuzz-target/x86_64-unknown-linux-gnu/release/<target> fuzz/artifacts/<target>/crash-…`.
  Minimize with `cargo +nightly fuzz tmin <target> <file>`. After fixing, add the reproducer to the crate's unit tests.
- For the 24-hour campaign, time alone is not considered sufficient (spec 15.3). Check unreached branches with
  `cargo +nightly fuzz coverage <target>` and `llvm-cov show`, and add seeds and harnesses. Save the corpus for every release (seeds for the next campaign).
- Randomness and cookie barriers: for the SRT listener, the harness writes the cookie back, so inputs reach past the connection setup. For the
  DTLS and TLS servers, the server's random values change the transcript, so a recorded client Finished fails verification (the main
  coverage is from ClientHello through key exchange). What comes after Finished (record layer, retransmission, KeyUpdate) is verified by the
  crate tests, or by adding a test entry point that lets the random source be replaced.

### Sanitizers (G4)

- Safe Rust code is fuzzed with AddressSanitizer (the `cargo fuzz` default).
- FFI (the codecs `senca-codec` loads with dlopen, and `senca-codec-worker`) runs in a separate process. For a release candidate, build the worker with
  `RUSTFLAGS=-Zsanitizer=address cargo +nightly build -Zbuild-std --target x86_64-unknown-linux-gnu -p senca-codec-worker`,
  run it through `tools/interop/codec_sw.sh` (crashes, hangs, malformed material) and check that nothing is reported.
  The external libraries themselves are not rebuilt with sanitizers (pin the distribution versions and list them in the SBOM).

## SBOM and licenses (G7)

```sh
tools/release/sbom.sh                       # out/release/sbom.cdx.json (CycloneDX 1.5)
python3 tools/release/license_check.py out/release/sbom.cdx.json \
  --exceptions tools/release/license-exceptions.txt --binary <senca-node> --binary <senca-codec-worker>
```

- SBOM contents: the Rust dependencies reachable from `senca-node` and `senca-codec-worker` (runtime ones as `scope: required`,
  build-only and proc-macro ones as `excluded`), the SPDX expressions from Cargo.toml, the SHA-256 from Cargo.lock, and for the
  Docker image (default `senca/node:local`, change with `IMAGE`) the dpkg list and the License fields of the copyright files.
- Allowed licenses: MIT, Apache-2.0, BSD-2-Clause, BSD-3-Clause, ISC, Unicode-3.0, Zlib, CC0-1.0.
  MPL-2.0 is REVIEW (file-level copyleft; acceptable if unmodified and the source location is stated; record the check in the exceptions file).
  Anything else, or no license stated, is FAIL. Exceptions go in `tools/release/license-exceptions.txt` (who checked, and when).
- Forbidden components (no exceptions possible; ADR-0004, AC-R7-03): software H.264/H.265/AAC codecs
  (openh264, x264, x265, libde265, fdk-aac, faad and so on) and FFmpeg (bindings, libav*). Rust crate names,
  OS package names, and the executables' NEEDED entries and the library names they may dlopen are all checked.
- OS component licenses are an informational list (the Debian field is not necessarily SPDX). GPL and LGPL components (glibc, bash and so on)
  are included in the image as separate works. State where to get their sources (Ubuntu source packages; the version is `cdx:deb:source` in
  the SBOM) in the release notes.

## Reproducible build and signing (G9)

```sh
REPRO_CHECK=1 SIGNING_KEY=/secure/senca-release.key tools/release/build.sh
```

What `tools/release/build.sh` does:

1. Extracts the sources with `git archive <commit>` (changes in the working tree and untracked files are not included).
2. Builds `senca-node` and `senca-codec-worker` with `SOURCE_DATE_EPOCH` = the commit time, `--remap-path-prefix` (sources, `CARGO_HOME`
   and target mapped to fixed paths), `-C codegen-units=1` (`CARGO_PROFILE_RELEASE_CODEGEN_UNITS=1`), `--locked` and `CARGO_INCREMENTAL=0`.
   With `REPRO_CHECK=1` it builds again in another directory and checks with `cmp` that the results match.
3. Builds the Docker image using the same archive as the context and saves it with `docker save | gzip -n` (skip with `SKIP_IMAGE=1`).
4. Runs the SBOM and license checks (G7).
5. Writes `BUILDINFO` (commit, SOURCE_DATE_EPOCH, rustc/cargo versions, RUSTFLAGS, the reproducibility result, locked).
6. Creates `SHA256SUMS` and signs it with an Ed25519 key (`openssl pkeyutl -sign -rawin`).

Output goes to `out/release/<version>-<commit>/`.

Assumptions and limits:

- Same rustc (the version in `BUILDINFO`) and same target. To reproduce on another machine, install the same toolchain version, run
  `REPRO_CHECK=1` from the same commit, and compare the executable lines of `SHA256SUMS`.
- The Docker image depends on apt packages and on when `rustup` fetched things, so the image digest is not byte-identical.
  The executables inside the image are rebuilt in the build stage, so pin it to the same toolchain version as the host build and compare the
  hashes of the executables extracted from the image with the lines in `SHA256SUMS` (the current Dockerfile does not pin the toolchain version).
- **Cargo.lock must match the committed one (`--locked`).** If it does not, the build stops.
  `ALLOW_UNLOCKED=1` is only for trials during development (`BUILDINFO` records `locked=no`; never publish such a build). The build stage of the
  Dockerfile also uses `--locked`.

### Published images (ghcr.io)

Pushing a `v*` tag makes `.github/workflows/docker.yml` build standard and slim (amd64, arm64) and gpu (amd64), push them to
`ghcr.io/kobesoft-labs/senca`, and move `<version>`, `<version>-slim`, `<version>-gpu` plus `latest`, `slim`, `gpu` (not for
pre-releases such as `-rc1`). Pushes to `main` update `edge`, `edge-slim` and `edge-gpu`. arm64 is built natively on GitHub's
arm64 runners (no QEMU). After pushing a tag, check that the `docker` workflow is green and that
`docker buildx imagetools inspect ghcr.io/kobesoft-labs/senca:<version>` lists both platforms.

### Signing key

- The production key (Ed25519, PKCS#8 PEM) is kept where the release manager controls it (an offline machine or an HSM) and passed in via `SIGNING_KEY`.
  To create one: `openssl genpkey -algorithm ed25519 -out senca-release.key`. Publish the public key
  (`openssl pkey -in senca-release.key -pubout -out senca-release.pub.pem`) through a channel separate from the artifacts, such as the
  web site or the release notes of the repository, together with its fingerprint (`openssl pkey -pubin -in senca-release.pub.pem -outform DER | sha256sum`).
- Without `SIGNING_KEY`, the script creates `~/.cache/senca-release-dev.key` and signs with it (for development; never publish).

### Verification by users

```sh
# 1. Signature (with the public key obtained through a separate channel)
openssl pkeyutl -verify -pubin -inkey senca-release.pub.pem -rawin -in SHA256SUMS -sigfile SHA256SUMS.sig
# -> "Signature Verified Successfully"
# 2. Checksums
sha256sum -c SHA256SUMS
# 3. Image
gunzip -c senca-node-image.tar.gz | docker load
```

## Load and long runs (G5, G6)

```sh
cargo build --release -p senca-node -p senca-codec-worker   # CARGO_TARGET_DIR is assumed to default to ~/.cache/senca-target-release
PUBLISHERS=4 VIEWERS=200 tools/loadgen/load.sh 60            # load (no churn)
PUBLISHERS=4 VIEWERS=100 CHURN=30 PUB_CHURN=45 tools/soak/soak.sh 259200   # 72 hours (warm-up defaults to 600 seconds)
```

- `tools/loadgen/loadgen.py` (Python standard library only): `scenario` creates streams through the API, starts FFmpeg RTMP senders
  (`-re -stream_loop -1 -c copy`; FFmpeg is used only as a test peer) and distributes viewers over signed HLS URLs.
  Viewers reload the playlist every half target duration and fetch new segments over keep-alive. It reports p50/p95/p99 of segment fetch
  time (from request to last byte), playlist fetch time, time to the first segment, the number of fetches slower than the segment duration,
  errors and Mbit/s. `--procs` splits the work over several processes. `viewers` alone can also be run from another machine.
- `tools/soak/soak.sh`: starts a node and writes RSS, CPU, fds, sockets and threads before load (idle-before), under load (load) and after
  load (idle-after) to `samples.csv` every `INTERVAL` seconds. The verdict comes from `loadgen.py analyze` (RSS growth after warm-up, the
  regression slope, the fd and socket difference before and after load). RSS grows until the live windows and the recording queues fill up,
  so make the warm-up longer than that (in a 3-minute test on a development machine it flattened after about 2 minutes: `docs/dev/conformance/performance.md`).
- AC-12 is judged in the reference environment of spec 13.1. Numbers from a development machine are only for checking that the tools work and for seeing trends.
