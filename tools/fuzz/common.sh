# tools/fuzz/*.sh の共通部分（source して使う）。
ROOT=$(cd "$(dirname "${BASH_SOURCE[0]}")/../.." && pwd)
CARGO=${CARGO:-$HOME/.cargo/bin/cargo}
CACHE=${XDG_CACHE_HOME:-$HOME/.cache}
FUZZ_TARGET_DIR=${FUZZ_TARGET_DIR:-$CACHE/senca-fuzz-target}
FUZZ_BIN_DIR=$FUZZ_TARGET_DIR/x86_64-unknown-linux-gnu/release
CORPUS=${CORPUS:-$ROOT/fuzz/corpus}
FUZZ_OUT=${FUZZ_OUT:-$ROOT/fuzz/artifacts}
# libFuzzer の 1 入力あたりの制限時間（秒）と RSS の上限
FUZZ_TIMEOUT=${FUZZ_TIMEOUT:-10}
FUZZ_RSS_MB=${FUZZ_RSS_MB:-2048}
# sanitizer: address（既定）・none（速さ優先）。FFI を含む経路は address で回す（仕様 15.3）
FUZZ_SANITIZER=${FUZZ_SANITIZER:-address}

fuzz_targets() {
  sed -n 's/^name = "\(.*\)"$/\1/p' "$ROOT/fuzz/Cargo.toml" | grep -v '^senca-fuzz$'
}

fuzz_build() {
  # nightly と cargo-fuzz が要る: rustup toolchain install nightly --profile minimal && cargo install cargo-fuzz
  (cd "$ROOT/fuzz" && PATH="$HOME/.cargo/bin:$PATH" "$CARGO" +nightly fuzz build -O \
     --sanitizer "$FUZZ_SANITIZER" --target-dir "$FUZZ_TARGET_DIR" "$@")
}
