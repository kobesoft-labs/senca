#!/usr/bin/env bash
# 配布物の SBOM（CycloneDX 1.5 JSON）を作る（仕様 9.5・AC-25）。
#
#   tools/release/sbom.sh [OUT]          # 既定 out/release/sbom.cdx.json
#   IMAGE=senca/node:0.1.0 tools/release/sbom.sh
#   SRC_ROOT=/path/to/git-archive tools/release/sbom.sh OUT   # build.sh が使う（配布物を作ったソースで数える）
#
# - Rust: cargo metadata（--locked）で senca-node・senca-codec-worker から辿った依存。ライセンスと Cargo.lock の checksum。
# - OS: Docker イメージ（IMAGE、既定 senca/node:local）があれば、その中の dpkg-query の一覧と copyright の License 欄。
#   イメージがなければ OS の部品は載せない（警告を出す。release では必ずイメージを作ってから実行する）。
set -euo pipefail
ROOT=$(cd "$(dirname "$0")/../.." && pwd)
CARGO=${CARGO:-$HOME/.cargo/bin/cargo}
CACHE=${XDG_CACHE_HOME:-$HOME/.cache}
OUT=${1:-$ROOT/out/release/sbom.cdx.json}
IMAGE=${IMAGE:-senca/node:local}
TRIPLE=${TRIPLE:-x86_64-unknown-linux-gnu}
SRC_ROOT=${SRC_ROOT:-$ROOT}
WORK=$(mktemp -d "$CACHE/senca-sbom.XXXXXX")
cleanup() { rm -rf "$WORK"; }
trap cleanup EXIT
mkdir -p "$(dirname "$OUT")"

"$CARGO" metadata --manifest-path "$SRC_ROOT/Cargo.toml" --format-version 1 --locked --filter-platform "$TRIPLE" >"$WORK/meta.json"
VERSION=$(python3 -I -c 'import json,sys; m=json.load(open(sys.argv[1])); print(next(p["version"] for p in m["packages"] if p["name"]=="senca-node"))' "$WORK/meta.json")
COMMIT=${COMMIT:-$(git -C "$ROOT" rev-parse HEAD 2>/dev/null || echo "")}
EPOCH=${SOURCE_DATE_EPOCH:-$(git -C "$ROOT" log -1 --format=%ct 2>/dev/null || echo 0)}

DPKG_ARGS=()
if command -v docker >/dev/null && docker image inspect "$IMAGE" >/dev/null 2>&1; then
  # 1 行 = パッケージ・版・arch・ソース・License 欄（| 区切り）
  docker run --rm --network none --entrypoint /bin/sh "$IMAGE" -c '
    dpkg-query -W -f "\${Package}\t\${Version}\t\${Architecture}\t\${source:Package}\n" | while IFS="	" read -r p v a s; do
      f=/usr/share/doc/$p/copyright
      l=""
      if [ -r "$f" ]; then l=$(sed -n "s/^License: *\([^ ]*\).*/\1/p" "$f" | sort -u | head -16 | tr "\n" "|"); fi
      printf "%s\t%s\t%s\t%s\t%s\n" "$p" "$v" "$a" "$s" "$l"
    done' >"$WORK/dpkg.tsv"
  DPKG_ARGS=(--dpkg "$WORK/dpkg.tsv" --image "$IMAGE")
  echo "sbom: OS の部品 $(wc -l <"$WORK/dpkg.tsv") 個（$IMAGE）"
else
  echo "sbom: 警告: イメージ $IMAGE がない。OS の部品を載せない" >&2
fi

python3 -I "$ROOT/tools/release/sbom.py" --metadata "$WORK/meta.json" --lock "$SRC_ROOT/Cargo.lock" \
  --root senca-node --root senca-codec-worker --version "$VERSION" --commit "$COMMIT" --epoch "$EPOCH" \
  "${DPKG_ARGS[@]}" -o "$OUT"
