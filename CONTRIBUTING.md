# Contributing to SENCA

English | [日本語](#日本語)

Thank you for your interest in SENCA. This document explains how to build and test the project, the rules the code follows, and how to
propose changes. The detailed rules are in [docs/dev/DEVELOPMENT.md](docs/dev/DEVELOPMENT.md) ([English](docs/dev/DEVELOPMENT.en.md)); the release
gate is in [docs/dev/RELEASE.md](docs/dev/RELEASE.md) ([English](docs/dev/RELEASE.en.md)).

SENCA is at v0.0.0. Interfaces may still change between versions.

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

Fuzzing, sanitizers, load and soak tests are part of the release gate rather than of everyday changes; see [docs/dev/RELEASE.en.md](docs/dev/RELEASE.en.md).

## Coding rules

A summary of [docs/dev/DEVELOPMENT.en.md](docs/dev/DEVELOPMENT.en.md):

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
- **Dependencies.** When adding one, update the list in ADR-0002 (`docs/dev/adr/0002-dependencies-and-crypto.md`). Software H.264/H.265/AAC codecs
  and FFmpeg must never become product dependencies (ADR-0004).
- **Language.** Documentation and code comments are written in Japanese; identifiers are in English.
- **Commits.** Do not put the names of tools used for the work, or co-author trailers, in commit messages or tracked files.

## Proposing changes

- Report bugs and propose features through issues on [github.com/kobesoft-labs/senca](https://github.com/kobesoft-labs/senca).
  For larger changes, open an issue first to discuss the design (significant design decisions are recorded as ADRs in `docs/dev/adr/`).
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

SENCA に関心を持っていただき、ありがとうございます。この文書では、ビルドとテストの方法、コードの規約、変更の提案方法を説明します。
詳しい規約は [docs/dev/DEVELOPMENT.md](docs/dev/DEVELOPMENT.md) に、リリース前の確認項目は [docs/dev/RELEASE.md](docs/dev/RELEASE.md) にあります。

SENCA の現在のバージョンは v0.0.0 です。インターフェースはバージョン間で変わる可能性があります。

### ビルド

SENCA は Rust のモノレポです（`crates/*`、edition 2024、Rust 1.90 以上。`Cargo.toml` の `rust-version` を参照）。

```sh
cargo build --workspace
```

JavaScript の SDK（`apps/sdk-js`）は、実行にビルドは不要です。テストと型の生成については [apps/sdk-js/README.md](apps/sdk-js/README.md) を参照してください。

### テスト

すべての変更で、次のコマンドが成功する必要があります。

```sh
cargo test --workspace
cargo clippy --workspace --all-targets -- -D warnings
```

#### 相互接続テスト（`tools/interop/*.sh`）

単体テストに加えて、独立した他の実装との相互接続も確認します。各スクリプトは必要なものを `cargo` でビルドし（多くは `KEEP=1` で
作業ディレクトリを残せます）、`PASS`・`FAIL`・`SKIP` の行と集計を出力します。外部ツールや Docker イメージがない場合はスキップされる
シナリオがあります。すべて成功したと判断する前に、`SKIP` の行を確認してください。

スクリプトが使う外部ツールは次のとおりです（テストの相手としてのみ使い、製品には含めません）。

- **FFmpeg・ffprobe**: 既定は `~/tools/ffmpeg`・`~/tools/ffprobe`（静的ビルド 7.0.2）です。環境変数 `FFMPEG`・`FFPROBE` で変更できます。
  多くのメディア系スクリプト（`rtmp_basic.sh`・`srt.sh`・`rtp_rtsp.sh`・`hls_playlist.sh`・`ts_fmp4.sh`・`node.sh`・`pipeline.sh`・
  `codec_sw.sh`・`image.sh`・`captions.sh`・`dtls_srtp.sh` など）が使います。
- **Docker** と次のイメージ（それぞれ環境変数で変更できます）:
  - ブラウザーのテスト（`rooms.sh`・`node_rooms.sh`・`node_media_pipelines.sh`・`webrtc.sh`・`sdk_js.sh`・`sender_web.sh`・`console.sh`・
    `quic_moq.sh`）用の、Google Chrome と `playwright-core` を含むイメージ: `BROWSER_IMAGE`（`node_webrtc.sh` では `CHROME_IMAGE`）、既定は
    `ov-chrome:latest`。これはローカルのイメージで、このリポジトリには定義がありません。Chrome と `playwright-core` を入れたイメージを
    作成し、その名前を指定してください。`console.sh` は、このイメージがない場合 `mcr.microsoft.com/playwright` を使います。
  - RTSP のテスト（`rtp_rtsp.sh`）用の mediamtx: `MEDIAMTX_IMAGE`、既定は `bluenviron/mediamtx:latest`。
  - STUN・TURN のテスト（`ice_turn.sh`）用の coturn: `COTURN_IMAGE`、既定は `coturn/coturn`。
  - `sdk_js.sh`・`sender_web.sh` 用の Node.js: `NODE_IMAGE`、既定は `node:22.23.4-bookworm-slim`。
  - 任意: SRT の AES-GCM のシナリオ用に、libsrt 1.5.4 の `srt-live-transmit` を含むイメージ（`SRT_GCM_IMAGE`。未設定ならスキップ）。
- **OpenSSL 3.x**（`tls.sh`・`dtls_srtp.sh`。`quic_moq.sh` は OpenSSL 3.5 の `s_client -quic` を使います）。
- **ソフトウェアコーデックのライブラリ**（`codec_sw.sh`・`node.sh` など）: 既定は `~/tools/codeclibs/usr/lib/x86_64-linux-gnu`
  （`SENCA_CODEC_LIB_DIR`・`CODEC_LIB_DIR` で変更）。`apt-get download libdav1d7 libvpx12 libopus0 libsvtav1enc2` で取得したパッケージを
  `dpkg -x` で展開したものです。
- `quic_moq.sh` は、aioquic（`AIOQUIC_PY`。未指定なら `uv` で `~/.cache` に作成）と moq-dev/moq（`MOQ_BIN_DIR`。または `MOQ_BUILD=1` で
  取得してビルド）も使います。
- `curl`・`python3` は多くのスクリプトで使います。`sdk_js.sh` はローカルの Node.js と、あれば `tsc`（`TSC=...`）も使います。

各スクリプトが何を確認し、どの環境変数を読むかは、スクリプト先頭のコメントに書かれています。

fuzz・sanitizer・負荷テスト・長時間テストは、日々の変更ではなくリリース前の確認で行います。[docs/dev/RELEASE.md](docs/dev/RELEASE.md) を参照してください。

### コードの規約

[docs/dev/DEVELOPMENT.md](docs/dev/DEVELOPMENT.md) の要点は次のとおりです。

- **sans-io の層構成。** プロトコル・コンテナ・ビットストリームの crate は、I/O・スレッド・時計を持ちません。バイト列を入力すると
  イベントと出力バイト列を返す状態機械として実装し、時刻は引数で渡します。I/O（tokio）は `senca-node` などの実行ファイル側だけで
  扱います。共通の型は `senca-core` にあります。
- **不正な入力は厳格に拒否する。** 長さ・整数演算・入れ子の深さ・総量に上限を設け、壊れた入力には `Err` を返します。パニック・
  無限ループ・無制限のメモリー確保はしません。`unsafe` は FFI の crate だけで許可し、理由と境界をコメントに書きます。
- **コピーを避ける。** payload は `bytes::Bytes` で受け取り、スライスで渡します。
- **独立した実装と照合する。** 単体テストに加えて、他の実装と照合するテストを `tools/interop/` に追加します。
- **暗号プリミティブは自作しない。** AES・SHA・HMAC・HKDF・署名・鍵交換・乱数は検証済みのライブラリを使います（ADR-0002）。
  プロトコル（TLS/DTLS/SRTP/QUIC のハンドシェイクとレコード層）はこのリポジトリで実装します。
- **秘密情報を出力しない。** ストリームキーやトークンを、ログ・エラー・`Debug` 表示に出しません。
- **依存関係。** 依存を追加するときは、ADR-0002（`docs/dev/adr/0002-dependencies-and-crypto.md`）の一覧を更新します。H.264/H.265/AAC の
  ソフトウェアコーデックと FFmpeg は、製品の依存にしません（ADR-0004）。
- **言語。** ドキュメントとコメントは日本語、識別子は英語で書きます。
- **コミット。** コミットメッセージや追跡対象のファイルに、作業に使ったツールの名前や共同作者の行を入れません。

### 変更の提案

- 不具合の報告や機能の提案は、[github.com/kobesoft-labs/senca](https://github.com/kobesoft-labs/senca) の issue で受け付けています。
  大きな変更は、先に issue で設計を相談してください（重要な設計の決定は `docs/dev/adr/` に ADR として記録します）。
- 変更は pull request で送ってください。1 つの pull request は 1 つの目的に絞り、テスト（単体テストと、独立した実装があれば相互接続
  テスト）を含めてください。`cargo test --workspace` と `cargo clippy --workspace --all-targets -- -D warnings` が成功することを確認し、
  実行した `tools/interop/` のスクリプトを記載してください。
- **セキュリティの脆弱性は公開の issue に書かないでください。** [SECURITY.md](SECURITY.md) を参照してください。

### 貢献のライセンス

SENCA は [Apache License 2.0](LICENSE) で提供しています。特に明示しない限り、SENCA に取り込まれることを意図して提出された貢献は、
追加の条件なしに Apache License 2.0 で提供されたものとみなします（inbound = outbound。ライセンス第 5 条のとおり）。この条件で
ライセンスする権利を持つものだけを提出してください。
