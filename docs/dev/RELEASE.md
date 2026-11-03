# リリースの手順と出荷 gate

[English](RELEASE.en.md) | 日本語

仕様 9.5（SBOM・依存ライセンス・署名付き release・再現可能な build）、15.2（AC-01・AC-12・AC-21・AC-25）、
15.3（セキュリティ出荷 gate）、REQ-007 AC-R7-03、REQ-010 R10-6 を、手で回せる手順と道具にしたもの。
道具は `tools/fuzz/`・`tools/release/`・`tools/loadgen/`・`tools/soak/`。結果の置き場所は `out/`（git に入れない）。

## 出荷 gate のチェックリスト

release 候補（commit を 1 つに決める）ごとに、すべてを満たすまで公開しない。結果・ログ・再現手順は
release evidence として保管する（仕様 15.3）。

| # | 項目 | 道具・手順 | 合格条件 |
|---|---|---|---|
| G1 | 単体・結合試験 | `cargo test --workspace`、`cargo clippy --workspace --all-targets -- -D warnings` | 全部通る |
| G2 | 相互接続 | `tools/interop/*.sh`（適合台帳 `docs/dev/conformance/ledger.md` の各行） | 台帳の状態が前の release から後退していない |
| G3 | fuzz（AC-01、15.3） | `tools/fuzz/all.sh 86400`（target ごとに 24 時間以上） | 全 target で crash・timeout・OOM なし。corpus・到達 edge・未到達領域・見つけたものと修正を記録 |
| G4 | sanitizer（FFI） | `FUZZ_SANITIZER=address`（既定）で G3。FFI を含む codec worker は下の「sanitizer」 | AddressSanitizer の報告なし |
| G5 | 負荷（AC-12、R10-6） | `tools/loadgen/load.sh`（仕様 13.1 の基準環境で、送り手・視聴者は別の機械） | 第 13 章の容量条件。欠落・遅延を報告 |
| G6 | 長時間（AC-21） | `tools/soak/soak.sh 259200`（72 時間、churn あり） | crash ゼロ、warm-up 後の RSS 増分 5% 以内、fd・socket の回収 |
| G7 | SBOM・ライセンス（AC-25、AC-R7-03） | `tools/release/sbom.sh`・`tools/release/license_check.py`（G9 の中で自動） | 許可外のライセンス・禁止部品なし。REVIEW は確認の記録あり |
| G8 | 独立レビュー（15.3） | 暗号プロトコル（TLS・DTLS・SRTP・SRT の暗号・QUIC）、認可（JWT・署名付き URL・トークン）、分散の所有権（fencing・mTLS） | 担当実装者と独立したレビューの報告書。Critical/High の未解決なし。残りは影響・緩和・責任者・期限を記録 |
| G9 | 再現可能な build と署名 | `REPRO_CHECK=1 SIGNING_KEY=… tools/release/build.sh` | 2 回の build がバイト一致、SHA256SUMS の署名を検証できる |
| G10 | 脆弱性の監視 | RustSec（`cargo audit` など）を SBOM に対して | 既知の脆弱性の対応を記録 |

`docs/dev/conformance/ledger.md` の機能ごとの状態（experimental / preview / production、仕様 15.4）は、
この gate の結果で決める。未達の性能値を保証値として公開しない。

## fuzz（G3・G4）

coverage-guided fuzz（cargo-fuzz / libFuzzer）。crate は `fuzz/`（ワークスペースの外。nightly が要る）。

```sh
rustup toolchain install nightly --profile minimal
cargo install cargo-fuzz
tools/fuzz/seed.sh                 # 種を fuzz/corpus/<target>/ に作る（試験データ・FFmpeg の出力・自前の往復・手書き）
tools/fuzz/run.sh rtmp_server 600  # 1 つを 10 分
tools/fuzz/all.sh 60               # 全部を 60 秒ずつ（CI の smoke）
PARALLEL=8 tools/fuzz/all.sh 86400 # release の gate: target ごとに 24 時間
```

- target（28 本）: `amf0` `flv` `rtmp_server` `mpegts` `fmp4` `hls` `http` `sdp` `stun` `rtp` `rtcp` `rtp_depack`
  `rtsp_client` `rtsp_message` `srt_listener` `srt_packet` `dtls_server` `h264` `h265` `aac` `av1` `opus` `record`
  `jwt` `png` `jpeg` `raft` `tls_server`。入力の形（先頭の選択バイト、データグラムの区切り）は各 `fuzz/fuzz_targets/*.rs` の先頭に書いてある。
- 制限: `-rss_limit_mb=2048`、1 入力 10 秒（`FUZZ_RSS_MB`・`FUZZ_TIMEOUT` で変える）。
- 結果: `fuzz/artifacts/summary.md`（target ごとの実行数・exec/s・cov（edge）・ft・corpus・peak RSS・再現入力）、
  ログは `fuzz/artifacts/logs/<target>.log`、再現入力は `fuzz/artifacts/<target>/crash-*`（`oom-*`・`timeout-*`）。
- 再現: `RUST_BACKTRACE=1 ~/.cache/senca-fuzz-target/x86_64-unknown-linux-gnu/release/<target> fuzz/artifacts/<target>/crash-…`。
  最小化は `cargo +nightly fuzz tmin <target> <file>`。直したら再現入力を crate の単体試験に入れる。
- 24 時間の campaign では、時間だけで十分としない（仕様 15.3）。`cargo +nightly fuzz coverage <target>` と
  `llvm-cov show` で未到達の分岐を確かめ、種・harness を足す。corpus は release ごとに保存する（次の campaign の種）。
- 乱数・cookie の壁: SRT の listener は cookie を harness が書き戻すので接続後まで届く。DTLS・TLS の server は
  server の乱数で transcript が変わるため、記録した client の Finished は検証で落ちる（ClientHello〜鍵交換までが主な対象）。
  Finished の先（記録層・再送・KeyUpdate）は crate の試験か、乱数を差し替えられる試験用の入口を足して確かめる。

### sanitizer（G4）

- Rust の安全なコードは AddressSanitizer で fuzz する（`cargo fuzz` の既定）。
- FFI（`senca-codec` の dlopen するコーデック、`senca-codec-worker`）は別プロセスで動く。release 候補では
  `RUSTFLAGS=-Zsanitizer=address cargo +nightly build -Zbuild-std --target x86_64-unknown-linux-gnu -p senca-codec-worker`
  で作った worker を `tools/interop/codec_sw.sh`（crash・hang・不正素材）に通し、報告がないことを確かめる。
  外部ライブラリ自体は sanitizer で作り直さない（ディストリビューションの版を固定し、SBOM に載せる）。

## SBOM とライセンス（G7）

```sh
tools/release/sbom.sh                       # out/release/sbom.cdx.json（CycloneDX 1.5）
python3 tools/release/license_check.py out/release/sbom.cdx.json \
  --exceptions tools/release/license-exceptions.txt --binary <senca-node> --binary <senca-codec-worker>
```

- SBOM の中身: `senca-node`・`senca-codec-worker` から辿った Rust の依存（実行時は `scope: required`、
  build・proc-macro だけのものは `excluded`）、Cargo.toml の SPDX 式、Cargo.lock の SHA-256、
  Docker イメージ（既定 `senca/node:local`、`IMAGE` で変える）の dpkg の一覧と copyright の License 欄。
- 許可するライセンス: MIT・Apache-2.0・BSD-2-Clause・BSD-3-Clause・ISC・Unicode-3.0・Zlib・CC0-1.0。
  MPL-2.0 は REVIEW（ファイル単位の copyleft。改変しない・ソースの所在を示すなら可。確認を例外ファイルに記録）。
  それ以外・記載なしは FAIL。例外は `tools/release/license-exceptions.txt`（誰がいつ確認したか）。
- 禁止部品（例外にできない。ADR-0004・AC-R7-03）: ソフトウェアの H.264/H.265/AAC コーデック
  （openh264・x264・x265・libde265・fdk-aac・faad など）と FFmpeg（bindings・libav*）。Rust の crate 名、
  OS のパッケージ名、実行ファイルの NEEDED と dlopen しうるライブラリ名を検査する。
- OS の部品のライセンスは参考の一覧（Debian の欄は SPDX とは限らない）。GPL・LGPL の部品（glibc・bash など）は
  別の著作物としてイメージに入る。ソースの入手先（Ubuntu のソースパッケージ、版は SBOM の `cdx:deb:source`）を
  release ノートに書く。

## 再現可能な build と署名（G9）

```sh
REPRO_CHECK=1 SIGNING_KEY=/secure/senca-release.key tools/release/build.sh
```

`tools/release/build.sh` がすること:

1. `git archive <commit>` でソースを取り出す（作業ツリーの変更・追跡していないファイルは入らない）。
2. `SOURCE_DATE_EPOCH`＝commit の時刻、`--remap-path-prefix`（ソース・`CARGO_HOME`・target を固定のパスへ）、
   `-C codegen-units=1`（`CARGO_PROFILE_RELEASE_CODEGEN_UNITS=1`）、`--locked`、`CARGO_INCREMENTAL=0` で
   `senca-node`・`senca-codec-worker` を build する。`REPRO_CHECK=1` なら別のディレクトリでもう一度作り、`cmp` で一致を確かめる。
3. 同じ archive を context にして Docker イメージを作り、`docker save | gzip -n` で保存する（`SKIP_IMAGE=1` で省く）。
4. SBOM とライセンスの検査（G7）。
5. `BUILDINFO`（commit・SOURCE_DATE_EPOCH・rustc/cargo の版・RUSTFLAGS・再現性の確認結果・locked）。
6. `SHA256SUMS` を作り、Ed25519 の鍵で署名する（`openssl pkeyutl -sign -rawin`）。

出力は `out/release/<version>-<commit>/`。

前提と限界:

- 同じ rustc（`BUILDINFO` の版）・同じ target。別の機械での再現は、同じ版の toolchain を入れて同じ commit から
  `REPRO_CHECK=1` を回し、`SHA256SUMS` の実行ファイルの行を比べる。
- Docker イメージは apt のパッケージ・`rustup` の取得時刻に依存するので、イメージの digest はバイト一致しない。
  イメージの中の実行ファイルは build 段で作り直されるので、ホストの build と同じ toolchain の版に固定したうえで、
  イメージから取り出した実行ファイルのハッシュを `SHA256SUMS` の行と比べる（今の Dockerfile は toolchain の版を固定していない）。
- **Cargo.lock は commit されたものと一致していること（`--locked`）。** 一致しなければ build は止まる。
  `ALLOW_UNLOCKED=1` は開発中の試しだけ（`BUILDINFO` に `locked=no`。公開しない）。Dockerfile の build 段も `--locked` で作る。

### 公開イメージ（ghcr.io）

`v*` のタグを push すると、`.github/workflows/docker.yml` が `ghcr.io/kobesoft-labs/senca` に standard・slim（amd64・arm64）と
gpu（amd64）を作って push し、`<版>`・`<版>-slim`・`<版>-gpu` と `latest`・`slim`・`gpu`（`-rc1` などの pre-release では付けない）を
付け替える。`main` への push は `edge`・`edge-slim`・`edge-gpu`。arm64 は GitHub の arm64 ランナーでそのまま作る（QEMU は使わない）。
タグを push したら Actions の `docker` が緑になったことと、`docker buildx imagetools inspect ghcr.io/kobesoft-labs/senca:<版>` に
2 つの platform があることを確かめる。

### 署名の鍵

- 本番の鍵（Ed25519、PKCS#8 PEM）は release 担当者の管理する場所（オフラインの機械・HSM）に置き、`SIGNING_KEY` で渡す。
  作り方: `openssl genpkey -algorithm ed25519 -out senca-release.key`。公開鍵
  （`openssl pkey -in senca-release.key -pubout -out senca-release.pub.pem`）は web サイト・リポジトリの release ノートなど、
  配布物とは別の経路で公開し、指紋（`openssl pkey -pubin -in senca-release.pub.pem -outform DER | sha256sum`）を併記する。
- `SIGNING_KEY` が無いときは `~/.cache/senca-release-dev.key` を作って署名する（開発用。公開しない）。

### 利用者の検証手順

```sh
# 1. 署名（公開鍵は別の経路で入手したもの）
openssl pkeyutl -verify -pubin -inkey senca-release.pub.pem -rawin -in SHA256SUMS -sigfile SHA256SUMS.sig
# → "Signature Verified Successfully"
# 2. チェックサム
sha256sum -c SHA256SUMS
# 3. イメージ
gunzip -c senca-node-image.tar.gz | docker load
```

## 負荷と長時間（G5・G6）

```sh
cargo build --release -p senca-node -p senca-codec-worker   # CARGO_TARGET_DIR は ~/.cache/senca-target-release を既定に想定
PUBLISHERS=4 VIEWERS=200 tools/loadgen/load.sh 60            # 負荷（churn なし）
PUBLISHERS=4 VIEWERS=100 CHURN=30 PUB_CHURN=45 tools/soak/soak.sh 259200   # 72 時間（warm-up 既定 600 秒）
```

- `tools/loadgen/loadgen.py`（Python 標準ライブラリだけ）: `scenario` は API で stream を作り、FFmpeg の RTMP 送信
  （`-re -stream_loop -1 -c copy`、FFmpeg は試験の相手としてだけ使う）を始め、署名付き HLS の URL に視聴者を振り分ける。
  視聴者は playlist を target duration の半分ごとに読み直し、新しい segment を keep-alive で取る。segment の取得時間
  （要求から最後のバイトまで）の p50/p95/p99、playlist の取得時間、最初の segment までの時間、segment の長さより
  取得が遅れた数、誤り、Mbit/s を出す。`--procs` で複数プロセスに分ける。`viewers` だけを別の機械から回すこともできる。
- `tools/soak/soak.sh`: ノードを起動し、負荷の前（idle-before）・負荷中（load）・負荷の後（idle-after）の RSS・CPU・fd・
  socket・スレッドを `INTERVAL` 秒ごとに `samples.csv` へ。判定は `loadgen.py analyze`（warm-up 後の RSS 増分、
  回帰の傾き、負荷の前後の fd・socket の差）。ライブの窓・録画の待ち行列が埋まるまで RSS は増えるので、
  warm-up はその時間より長くとる（開発機の 3 分の試験では約 2 分で平らになった: `docs/dev/conformance/performance.md`）。
- AC-12 の判定は仕様 13.1 の基準環境で行う。開発機の数字は道具の動作確認と傾向の把握のためのもの。
