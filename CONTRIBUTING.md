# Contributing to SENCA

English | [日本語](#日本語)

Thank you for your interest in SENCA. This document explains how to build and test the project, the rules the code follows, and how to
propose changes. The detailed rules are in [docs/DEVELOPMENT.md](docs/DEVELOPMENT.md) ([English](docs/DEVELOPMENT.en.md)); the release
gate is in [docs/RELEASE.md](docs/RELEASE.md) ([English](docs/RELEASE.en.md)).

SENCA is at v0.0.0 (experimental). Interfaces may still change without notice.

## Building

SENCA is a Rust monorepo (`crates/*`, edition 2024, minimum Rust 1.90; see `rust-version` in `Cargo.toml`).

```sh
cargo build --workspace
```

The JavaScript SDK (`apps/sdk-js`) has no build step for running; see [apps/sdk-js/README.en.md](apps/sdk-js/README.en.md) for its tests and type generation.

## Testing

Every change must pass:

```sh
cargo test --workspace
cargo clippy --workspace --all-targets -- -D warnings
```

### Interoperability tests (`tools/interop/*.sh`)

Each implementation is also checked against independent implementations. The scripts build what they need with `cargo` (most accept
`KEEP=1` to keep the work directory) and print `PASS` / `FAIL` / `SKIP` lines with a summary. Several scenarios are skipped when the external
tool or Docker image is missing, so read the `SKIP` lines before concluding that everything passed.

External tools used by the scripts (used only as test peers; none of them is part of the product):

- **FFmpeg / ffprobe**: `~/tools/ffmpeg` and `~/tools/ffprobe` (static build 7.0.2) by default; override with the `FFMPEG` and `FFPROBE`
  environment variables. Used by most media scripts (`rtmp_basic.sh`, `srt.sh`, `rtp_rtsp.sh`, `hls_playlist.sh`, `ts_fmp4.sh`, `node.sh`,
  `pipeline.sh`, `codec_sw.sh`, `image.sh`, `captions.sh`, `dtls_srtp.sh` and others).
- **Docker**, with these images (each can be overridden by an environment variable):
  - A Chrome image with Google Chrome and `playwright-core`, for the browser tests (`rooms.sh`, `node_rooms.sh`, `node_media_pipelines.sh`,
    `webrtc.sh`, `sdk_js.sh`, `sender_web.sh`, `console.sh`, `quic_moq.sh`): `BROWSER_IMAGE` (`CHROME_IMAGE` for `node_webrtc.sh`), default
    `ov-chrome:latest`. That image is a local image and is not defined in this repository; build your own with Chrome and `playwright-core`
    and pass its name. `console.sh` falls back to `mcr.microsoft.com/playwright`.
  - mediamtx for the RTSP tests (`rtp_rtsp.sh`): `MEDIAMTX_IMAGE`, default `bluenviron/mediamtx:latest`.
  - coturn for the STUN/TURN tests (`ice_turn.sh`): `COTURN_IMAGE`, default `coturn/coturn`.
  - Node.js for `sdk_js.sh` and `sender_web.sh`: `NODE_IMAGE`, default `node:22.23.4-bookworm-slim`.
  - Optional: an image with libsrt 1.5.4 `srt-live-transmit` for the SRT AES-GCM scenario (`SRT_GCM_IMAGE`; skipped if unset).
- **OpenSSL 3.x** for `tls.sh` and `dtls_srtp.sh` (`quic_moq.sh` uses OpenSSL 3.5 `s_client -quic`).
- **Software codec libraries** for `codec_sw.sh`, `node.sh` and others: `~/tools/codeclibs/usr/lib/x86_64-linux-gnu` by default
  (`SENCA_CODEC_LIB_DIR` / `CODEC_LIB_DIR`), made by running `apt-get download libdav1d7 libvpx12 libopus0 libsvtav1enc2` and extracting
  them with `dpkg -x`.
- `quic_moq.sh` additionally uses aioquic (`AIOQUIC_PY`, or created with `uv` under `~/.cache`) and moq-dev/moq (`MOQ_BIN_DIR`, or
  `MOQ_BUILD=1` to fetch and build it).
- `curl` and `python3` are used throughout; `sdk_js.sh` also uses a local Node.js and, if available, `tsc` (`TSC=...`).

The header comment of each script lists exactly what it checks and which variables it reads.

Fuzzing, sanitizers, load and soak tests are part of the release gate rather than of everyday changes; see [docs/RELEASE.en.md](docs/RELEASE.en.md).

## Coding rules

A summary of [docs/DEVELOPMENT.en.md](docs/DEVELOPMENT.en.md):

- **sans-io layering.** Protocol, container and bitstream crates have no I/O, threads or clocks: they are state machines that take bytes and
  return events and output bytes, with time passed in as an argument. I/O (tokio) lives only in executables such as `senca-node`. Shared types
  are in `senca-core`.
- **Strict parsing.** Bound lengths, integer arithmetic, nesting and totals, and return `Err` for broken input. No panics, infinite loops or
  unbounded allocation. `unsafe` is not allowed except in FFI crates, which must document the reason and the boundary.
- **Zero-copy.** Take payloads as `bytes::Bytes` and pass slices on.
- **Verify against independent implementations.** Add tests under `tools/interop/` that compare with other implementations, in addition to unit tests.
- **No hand-written cryptographic primitives.** AES, SHA, HMAC, HKDF, signatures, key exchange and random numbers come from vetted libraries
  (ADR-0002). The protocols themselves (TLS/DTLS/SRTP/QUIC handshakes and record layers) are implemented in-house.
- **Do not leak secrets.** Stream keys and tokens must not appear in logs, errors or `Debug` output.
- **Dependencies.** When adding one, update the list in ADR-0002 (`docs/adr/0002-dependencies-and-crypto.md`). Software H.264/H.265/AAC codecs
  and FFmpeg must never become product dependencies (ADR-0004).
- **Language.** Documentation and code comments are written in Japanese; identifiers are in English.
- **Commits.** Do not put the names of tools used for the work, or co-author trailers, in commit messages or tracked files.

## Proposing changes

- Report bugs and propose features through issues on [github.com/kobesoft-labs/senca](https://github.com/kobesoft-labs/senca).
  For larger changes, open an issue first to discuss the design (significant design decisions are recorded as ADRs in `docs/adr/`).
- Send changes as pull requests. Keep each pull request focused, include tests (unit tests, plus an interop test where an independent
  implementation exists), and make sure `cargo test --workspace` and `cargo clippy --workspace --all-targets -- -D warnings` pass.
  Mention which `tools/interop/` scripts you ran.
- **Do not report security vulnerabilities in public issues.** See [SECURITY.md](SECURITY.md).

## License of contributions

SENCA is licensed under the [Apache License 2.0](LICENSE). Unless you explicitly state otherwise, any contribution you intentionally submit
for inclusion in SENCA is licensed under the Apache License 2.0, without any additional terms or conditions (inbound = outbound, as in
section 5 of the license). Only submit work that you have the right to license this way.

---

## 日本語

[English](#contributing-to-senca) | 日本語

SENCA に関心を持ってくださってありがとうございます。この文書は、作り方・試験の仕方・コードの決まり・変更の提案の仕方をまとめたもの。
詳しい決まりは [docs/DEVELOPMENT.md](docs/DEVELOPMENT.md)、出荷 gate は [docs/RELEASE.md](docs/RELEASE.md)。

SENCA は v0.0.0（experimental）。インターフェースは予告なく変わることがある。

### 作り方

Rust のモノレポ（`crates/*`、edition 2024、Rust 1.90 以上。`Cargo.toml` の `rust-version`）。

```sh
cargo build --workspace
```

JavaScript の SDK（`apps/sdk-js`）は動かすのにビルドが要らない。試験と型の生成は [apps/sdk-js/README.md](apps/sdk-js/README.md)。

### 試験

すべての変更で次が通ること。

```sh
cargo test --workspace
cargo clippy --workspace --all-targets -- -D warnings
```

#### 相互接続試験（`tools/interop/*.sh`）

独立した別の実装とも突き合わせる。スクリプトは要るものを `cargo` で作り（多くは `KEEP=1` で作業場所を残す）、`PASS`・`FAIL`・`SKIP` の行と
まとめを出す。外部の道具や Docker イメージが無いと飛ばす場面があるので、全部通ったと判断する前に `SKIP` の行を見ること。

スクリプトが使う外部の道具（試験の相手としてだけ使い、製品には入れない）:

- **FFmpeg・ffprobe**: 既定は `~/tools/ffmpeg`・`~/tools/ffprobe`（静的ビルド 7.0.2）。`FFMPEG`・`FFPROBE` 環境変数で上書きできる。
  メディアのスクリプトの多く（`rtmp_basic.sh`・`srt.sh`・`rtp_rtsp.sh`・`hls_playlist.sh`・`ts_fmp4.sh`・`node.sh`・`pipeline.sh`・
  `codec_sw.sh`・`image.sh`・`captions.sh`・`dtls_srtp.sh` など）が使う。
- **Docker** と次のイメージ（それぞれ環境変数で替えられる）:
  - ブラウザーの試験（`rooms.sh`・`node_rooms.sh`・`node_media_pipelines.sh`・`webrtc.sh`・`sdk_js.sh`・`sender_web.sh`・`console.sh`・
    `quic_moq.sh`）の Google Chrome と `playwright-core` の入ったイメージ: `BROWSER_IMAGE`（`node_webrtc.sh` は `CHROME_IMAGE`）、既定
    `ov-chrome:latest`。これは手元のイメージで、このリポジトリには定義が無い。Chrome と `playwright-core` を入れたイメージを作って名前を渡す。
    `console.sh` は無ければ `mcr.microsoft.com/playwright` を使う。
  - RTSP の試験（`rtp_rtsp.sh`）の mediamtx: `MEDIAMTX_IMAGE`、既定 `bluenviron/mediamtx:latest`。
  - STUN・TURN の試験（`ice_turn.sh`）の coturn: `COTURN_IMAGE`、既定 `coturn/coturn`。
  - `sdk_js.sh`・`sender_web.sh` の Node.js: `NODE_IMAGE`、既定 `node:22.23.4-bookworm-slim`。
  - 任意: SRT の AES-GCM の場面に、libsrt 1.5.4 の `srt-live-transmit` の入ったイメージ（`SRT_GCM_IMAGE`。無ければ飛ばす）。
- **OpenSSL 3.x**（`tls.sh`・`dtls_srtp.sh`。`quic_moq.sh` は OpenSSL 3.5 の `s_client -quic`）。
- **ソフトウェアコーデックのライブラリ**（`codec_sw.sh`・`node.sh` など）: 既定 `~/tools/codeclibs/usr/lib/x86_64-linux-gnu`
  （`SENCA_CODEC_LIB_DIR`・`CODEC_LIB_DIR`）。`apt-get download libdav1d7 libvpx12 libopus0 libsvtav1enc2` を `dpkg -x` したもの。
- `quic_moq.sh` は aioquic（`AIOQUIC_PY`、無ければ `uv` で `~/.cache` に作る）と moq-dev/moq（`MOQ_BIN_DIR`、または `MOQ_BUILD=1` で取ってきて作る）も使う。
- `curl`・`python3` はあちこちで使う。`sdk_js.sh` は手元の Node.js と、あれば `tsc`（`TSC=...`）も使う。

何を確かめ、どの変数を読むかは各スクリプトの先頭のコメントに書いてある。

fuzz・sanitizer・負荷・長時間の試験は普段の変更ではなく出荷 gate の一部。[docs/RELEASE.md](docs/RELEASE.md) を見ること。

### コードの決まり

[docs/DEVELOPMENT.md](docs/DEVELOPMENT.md) の要約:

- **sans-io の層。** プロトコル・コンテナ・ビットストリームの crate は I/O・スレッド・時計を持たない。バイト列を入れるとイベントや出力バイト列が
  返る状態機械にし、時刻は引数で渡す。I/O（tokio）は `senca-node` などの実行ファイル側だけ。共通の型は `senca-core`。
- **厳格に拒否する。** 長さ・整数演算・入れ子・総量に上限を置き、壊れた入力は `Err` を返す。パニック・無限ループ・無制限の確保をしない。
  `unsafe` は FFI の crate だけ例外（理由と境界をコメントに書く）。
- **コピーしない。** payload は `bytes::Bytes` で受け、切り出しで渡す。
- **独立実装で確かめる。** 単体試験に加えて、別の実装と突き合わせる試験を `tools/interop/` に置く。
- **暗号の部品は自分で書かない。** AES・SHA・HMAC・HKDF・署名・鍵交換・乱数は検証済みのライブラリ（ADR-0002）。プロトコル
  （TLS/DTLS/SRTP/QUIC のハンドシェイクや記録層）は自前。
- **秘密を出さない。** stream key・トークンはログ・エラー・`Debug` 表示に出さない。
- **依存。** 足すときは ADR-0002（`docs/adr/0002-dependencies-and-crypto.md`）の一覧を更新する。ソフトウェアの H.264/H.265/AAC コーデックと
  FFmpeg は製品の依存にしない（ADR-0004）。
- **言語。** 文書・コメントは日本語、識別子は英語。
- **コミット。** コミットのメッセージや追跡するファイルに、作業に使った道具の名前や共同作者の行を残さない。

### 変更の提案

- 不具合の報告・機能の提案は [github.com/kobesoft-labs/senca](https://github.com/kobesoft-labs/senca) の issue へ。
  大きな変更は先に issue で設計を相談する（大きな設計の判断は `docs/adr/` の ADR に残す）。
- 変更は pull request で送る。1 つの pull request は 1 つの目的に絞り、試験（単体試験と、独立実装があれば相互接続試験）を付け、
  `cargo test --workspace` と `cargo clippy --workspace --all-targets -- -D warnings` が通ることを確かめる。回した `tools/interop/` のスクリプトを書く。
- **セキュリティの脆弱性は公開の issue に書かない。** [SECURITY.md](SECURITY.md) を見ること。

### 貢献のライセンス

SENCA は [Apache License 2.0](LICENSE) で提供する。特に断らない限り、SENCA に取り込まれることを意図して送られた貢献は、追加の条件なしに
Apache License 2.0 で提供されたものとする（inbound = outbound。ライセンスの第 5 条のとおり）。この形でライセンスする権利を持つものだけを送ること。
