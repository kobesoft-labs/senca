#!/usr/bin/env bash
# release の配布物を作る（仕様 9.5: 再現可能な build・SBOM・署名）。手順と検証は docs/dev/RELEASE.md。
#
#   tools/release/build.sh                    # HEAD から作る（作業中の変更は入らない）
#   SIGNING_KEY=/secure/senca-release.key tools/release/build.sh
#   REPRO_CHECK=1 tools/release/build.sh      # 別のディレクトリでもう一度 build し、バイト一致を確かめる
#   SKIP_IMAGE=1 tools/release/build.sh       # Docker イメージを作らない
#   ALLOW_UNLOCKED=1 tools/release/build.sh   # 開発中の試しだけ: Cargo.lock が古くても作る（--locked を外す。
#                                             # BUILDINFO に locked=no と書き、公開する release には使わない）
#
# 出力: out/release/<version>-<commit>/
#   senca-node  senca-codec-worker           x86_64-unknown-linux-gnu の実行ファイル
#   senca-node-image.tar.gz                  Docker イメージ（docker save。SKIP_IMAGE=1 なら無し）
#   sbom.cdx.json  license-check.txt         SBOM（CycloneDX 1.5）とライセンス検査の結果
#   BUILDINFO                                commit・SOURCE_DATE_EPOCH・rustc・RUSTFLAGS
#   SHA256SUMS  SHA256SUMS.sig  release-signing.pub.pem    チェックサムと Ed25519 の署名（openssl pkeyutl -rawin）
set -euo pipefail
shopt -s inherit_errexit
ROOT=$(cd "$(dirname "$0")/../.." && pwd)
CARGO=${CARGO:-$HOME/.cargo/bin/cargo}
RUSTC=${RUSTC:-$HOME/.cargo/bin/rustc}
CACHE=${XDG_CACHE_HOME:-$HOME/.cache}
TRIPLE=x86_64-unknown-linux-gnu
REF=${REF:-HEAD}

COMMIT=$(git -C "$ROOT" rev-parse "$REF")
SHORT=${COMMIT:0:12}
export SOURCE_DATE_EPOCH=${SOURCE_DATE_EPOCH:-$(git -C "$ROOT" log -1 --format=%ct "$COMMIT")}
WORK=$(mktemp -d "$CACHE/senca-release.XXXXXX")
cleanup() { rm -rf "$WORK"; }
trap cleanup EXIT

if ! git -C "$ROOT" diff --quiet HEAD 2>/dev/null; then
  echo "build: 注意: 作業ツリーに commit していない変更がある（配布物には入らない。$REF の内容で作る）" >&2
fi

# 1. ソースを git archive から取り出す（追跡しているファイルだけ・時刻は commit の時刻）
SRC_TAR=$WORK/senca-src.tar
git -C "$ROOT" archive --format=tar --prefix=senca/ "$COMMIT" >"$SRC_TAR"

# 2. 実行ファイルを再現可能に build する。
#    - パスは /build/senca・/cargo に書き換える（--remap-path-prefix）。build する場所・利用者名に依らない
#    - codegen-units=1（並列の分割による差を無くす）、--locked（Cargo.lock のとおり）
#    - 同じ rustc（rust-toolchain の版を BUILDINFO に残す）・同じ target の triple が前提
build_bins() {  # $1 = 作業ディレクトリ
  local d=$1
  mkdir -p "$d"
  tar -xf "$SRC_TAR" -C "$d"
  local src=$d/senca
  local cargo_home=${CARGO_HOME:-$HOME/.cargo}
  (
    cd "$src"
    export CARGO_TARGET_DIR=$d/target
    export CARGO_PROFILE_RELEASE_CODEGEN_UNITS=1
    export CARGO_INCREMENTAL=0
    export RUSTFLAGS="--remap-path-prefix=$src=/build/senca --remap-path-prefix=$cargo_home=/cargo --remap-path-prefix=$d/target=/build/target -C codegen-units=1 -C link-arg=-Wl,--build-id=sha1"
    export TZ=UTC LC_ALL=C
    "$CARGO" build --release $LOCKED_FLAG --target "$TRIPLE" -p senca-node -p senca-codec-worker
  ) || return 1
  echo "$d/target/$TRIPLE/release"
}

LOCKED_FLAG=--locked
if [[ "${ALLOW_UNLOCKED:-0}" == 1 ]]; then
  LOCKED_FLAG=
  echo "build: 注意: ALLOW_UNLOCKED=1（--locked なし）。公開する release には使わない" >&2
fi
echo "== build（commit $SHORT、SOURCE_DATE_EPOCH=$SOURCE_DATE_EPOCH）"
BIN_DIR=$(build_bins "$WORK/a" | tail -1)
VERSION=$(sed -n 's/^version = "\(.*\)"$/\1/p' "$WORK/a/senca/Cargo.toml" | head -1)
OUT=${OUT:-$ROOT/out/release/$VERSION-$SHORT}
rm -rf "$OUT"
mkdir -p "$OUT"
for b in senca-node senca-codec-worker; do
  install -m 0755 "$BIN_DIR/$b" "$OUT/$b"
  touch -d "@$SOURCE_DATE_EPOCH" "$OUT/$b"
done

REPRO="未確認"
if [[ "${REPRO_CHECK:-0}" == 1 ]]; then
  echo "== 再現性の確認（別のディレクトリでもう一度）"
  BIN_B=$(build_bins "$WORK/second-build-elsewhere" | tail -1)
  REPRO="一致"
  for b in senca-node senca-codec-worker; do
    if ! cmp -s "$BIN_DIR/$b" "$BIN_B/$b"; then
      REPRO="不一致"
      echo "build: FAIL: $b が一致しない（$(sha256sum "$BIN_DIR/$b" | cut -c1-16) / $(sha256sum "$BIN_B/$b" | cut -c1-16)）" >&2
    fi
  done
  echo "build: 再現性: $REPRO"
  [[ $REPRO == 一致 ]] || exit 1
fi

# 3. Docker イメージ（git archive をそのまま build context にする）
IMAGE=senca/node:$VERSION-$SHORT
if [[ "${SKIP_IMAGE:-0}" != 1 ]]; then
  echo "== Docker イメージ $IMAGE"
  # context は senca/ の中身（prefix を外し、時刻・所有者を固定して並べ直す）
  mkdir -p "$WORK/ctx"
  tar -xf "$SRC_TAR" -C "$WORK/ctx"
  tar -C "$WORK/ctx/senca" --sort=name --mtime="@$SOURCE_DATE_EPOCH" --owner=0 --group=0 --numeric-owner \
    -cf "$WORK/ctx.tar" .
  docker build --build-arg SOURCE_DATE_EPOCH="$SOURCE_DATE_EPOCH" -t "$IMAGE" - <"$WORK/ctx.tar"
  docker save "$IMAGE" | gzip -n >"$OUT/senca-node-image.tar.gz"
  docker image inspect --format '{{.Id}}' "$IMAGE" >"$OUT/senca-node-image.id"
fi

# 4. SBOM とライセンスの検査（AC-25）
echo "== SBOM・ライセンス"
SBOM_IMAGE=$IMAGE
[[ "${SKIP_IMAGE:-0}" != 1 ]] || SBOM_IMAGE=${SBOM_IMAGE_FALLBACK:-senca/node:local}
SRC_ROOT=$WORK/a/senca COMMIT=$COMMIT IMAGE=$SBOM_IMAGE "$ROOT/tools/release/sbom.sh" "$OUT/sbom.cdx.json"
set +e
python3 -I "$ROOT/tools/release/license_check.py" "$OUT/sbom.cdx.json" \
  --exceptions "$ROOT/tools/release/license-exceptions.txt" \
  --binary "$OUT/senca-node" --binary "$OUT/senca-codec-worker" >"$OUT/license-check.txt"
LC=$?
set -e
tail -1 "$OUT/license-check.txt"
[[ $LC == 0 ]] || { echo "build: FAIL: ライセンス検査（$OUT/license-check.txt）" >&2; exit 1; }

# 5. BUILDINFO
{
  echo "commit=$COMMIT"
  echo "version=$VERSION"
  echo "source_date_epoch=$SOURCE_DATE_EPOCH"
  echo "rustc=$("$RUSTC" --version)"
  echo "cargo=$("$CARGO" --version)"
  echo "target=$TRIPLE"
  echo "rustflags=--remap-path-prefix=<src>=/build/senca --remap-path-prefix=<cargo_home>=/cargo --remap-path-prefix=<target>=/build/target -C codegen-units=1 -C link-arg=-Wl,--build-id=sha1"
  echo "profile=release (Cargo.toml [profile.release]: lto=thin, debug=1) + CARGO_PROFILE_RELEASE_CODEGEN_UNITS=1"
  echo "reproducibility_check=$REPRO"
  echo "locked=$([[ -n "$LOCKED_FLAG" ]] && echo yes || echo no)"
  [[ -f "$OUT/senca-node-image.id" ]] && echo "image=$IMAGE $(cat "$OUT/senca-node-image.id")"
} >"$OUT/BUILDINFO"
touch -d "@$SOURCE_DATE_EPOCH" "$OUT"/*

# 6. チェックサムと署名（Ed25519、openssl pkeyutl -rawin）
(cd "$OUT" && find . -maxdepth 1 -type f ! -name 'SHA256SUMS*' ! -name '*.pub.pem' -printf '%f\n' | LC_ALL=C sort \
  | xargs sha256sum >SHA256SUMS)
KEY=${SIGNING_KEY:-}
if [[ -z "$KEY" ]]; then
  KEY=$CACHE/senca-release-dev.key
  if [[ ! -f "$KEY" ]]; then
    openssl genpkey -algorithm ed25519 -out "$KEY"
    chmod 600 "$KEY"
  fi
  echo "build: 注意: SIGNING_KEY が無いので開発用の鍵（$KEY）で署名する。公開する release には使わない" >&2
fi
openssl pkey -in "$KEY" -pubout -out "$OUT/release-signing.pub.pem"
openssl pkeyutl -sign -inkey "$KEY" -rawin -in "$OUT/SHA256SUMS" -out "$OUT/SHA256SUMS.sig"

# 7. 検証（利用者と同じ手順）
openssl pkeyutl -verify -pubin -inkey "$OUT/release-signing.pub.pem" -rawin -in "$OUT/SHA256SUMS" \
  -sigfile "$OUT/SHA256SUMS.sig" >/dev/null
(cd "$OUT" && sha256sum --quiet -c SHA256SUMS)
echo "PASS: $OUT（署名・チェックサムを検証済み。公開鍵の指紋: $(openssl pkey -pubin -in "$OUT/release-signing.pub.pem" -outform DER | sha256sum | cut -c1-16)）"
ls -l "$OUT"
