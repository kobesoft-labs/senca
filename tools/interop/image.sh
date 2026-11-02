#!/usr/bin/env bash
# senca-image の PNG / JPEG と FFmpeg の突き合わせ。
#
#   - 自前の JPEG（4:4:4・4:2:0、q95）を FFmpeg で復号し、元画像との PSNR（40 dB 以上）と、
#     自前の復号との PSNR（45 dB 以上）を確かめる。
#   - 自前の PNG（RGB・RGBA・グレー）を FFmpeg で復号し、画素が完全に一致すること。
#   - FFmpeg が作った PNG（rgb24・rgba・gray・rgb48be・pal8・monob・ya8・gray16be、各種フィルター）を
#     自前で復号し、FFmpeg の復号と一致すること（16 bit は丸めの違いで ±1）。
#   - FFmpeg が作った JPEG（yuvj420p・yuvj422p・yuvj444p・gray）を自前で復号し、輝度の PSNR が 45 dB 以上。
#
# 本体は crates/senca-image/tests/ffmpeg_interop.rs（#[ignore] の試験）。ここではそれを動かして結果をまとめる。
# 環境変数: FFMPEG（既定 ~/tools/ffmpeg）、CARGO、CARGO_TARGET_DIR。
set -euo pipefail

export FFMPEG=${FFMPEG:-$HOME/tools/ffmpeg}
ROOT=$(cd "$(dirname "$0")/../.." && pwd)
CARGO=${CARGO:-cargo}
command -v "$CARGO" >/dev/null || CARGO=$HOME/.cargo/bin/cargo

WORK=$(mktemp -d)
cleanup() { rm -rf "$WORK"; }
trap cleanup EXIT

if [[ ! -x "$FFMPEG" ]]; then
  echo "FAIL ffmpeg が見つからない: $FFMPEG"
  exit 1
fi

PASS=0
FAIL=0

set +e
(cd "$ROOT" && "$CARGO" test -p senca-image --test ffmpeg_interop -- --ignored --show-output --test-threads=1) >"$WORK/out.log" 2>&1
STATUS=$?
set -e

while read -r line; do
  name=$(sed -E 's/^test ([^ ]+) \.\.\. .*/\1/' <<<"$line")
  if [[ "$line" == *"... ok" ]]; then
    echo "PASS $name"
    PASS=$((PASS + 1))
  else
    echo "FAIL $name"
    FAIL=$((FAIL + 1))
  fi
done < <(grep -E '^test [^ ]+ \.\.\. ' "$WORK/out.log" || true)

# 測った値（PSNR など）も出しておく。
grep -E 'dB|一致' "$WORK/out.log" | sed 's/^/  /' || true

if [[ $STATUS -ne 0 && $FAIL -eq 0 ]]; then
  echo "FAIL cargo test が失敗した"
  tail -n 30 "$WORK/out.log"
  FAIL=$((FAIL + 1))
fi
if [[ $PASS -eq 0 && $FAIL -eq 0 ]]; then
  echo "FAIL 試験が 1 つも動かなかった"
  FAIL=1
fi

echo "PASS: $PASS  FAIL: $FAIL"
[[ $FAIL -eq 0 ]]
