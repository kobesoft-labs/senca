#!/usr/bin/env bash
# fuzz の種（seed corpus）を fuzz/corpus/<target>/ に作る（git には入れない。.gitignore 済み）。
#
#   tools/fuzz/seed.sh            # 既存の種に足す
#   CLEAN=1 tools/fuzz/seed.sh    # 作り直す
#
# 種の出どころ:
#   1. crates/*/tests/data の試験データ（H.264・H.265・AV1・AAC・Opus・TS・MP4）
#   2. FFmpeg（試験相手としてだけ使う: ~/tools/ffmpeg）の出力（FLV・TS・fMP4・HLS・PNG・JPEG・ADTS・Ogg Opus・RTP・SDP）
#   3. 自前の client と server の往復（tools/fuzz/seedgen: RTMP・SRT・DTLS・TLS・STUN・JWT・Raft・画像・録画の索引）
#   4. 手書きの種（HTTP・RTSP・SDP・HLS。tools/fuzz/seed_helpers.py）
set -euo pipefail

ROOT=$(cd "$(dirname "$0")/../.." && pwd)
CORPUS=${CORPUS:-$ROOT/fuzz/corpus}
FFMPEG=${FFMPEG:-$HOME/tools/ffmpeg}
CARGO=${CARGO:-$HOME/.cargo/bin/cargo}
CACHE=${XDG_CACHE_HOME:-$HOME/.cache}
WORK=$(mktemp -d "$CACHE/senca-fuzz-seed.XXXXXX")
cleanup() { rm -rf "$WORK"; }
trap cleanup EXIT

PY="python3 -I $ROOT/tools/fuzz/seed_helpers.py"

if [[ "${CLEAN:-0}" == 1 ]]; then rm -rf "$CORPUS"; fi
mkdir -p "$CORPUS"

echo "== 試験データ"
$PY codec "$CORPUS" "$ROOT"/crates/*/tests/data/*
$PY ogg "$CORPUS" "$ROOT"/crates/senca-opus/tests/data/*.opus
$PY adts "$CORPUS" "$ROOT"/crates/senca-aac/tests/data/*.aac
$PY av1rtp "$CORPUS" "$ROOT"/crates/senca-av1/tests/data/*.obu

echo "== FFmpeg の出力"
if [[ -x "$FFMPEG" ]]; then
  ff() { "$FFMPEG" -hide_banner -loglevel error -y "$@" || echo "seed: FFmpeg が失敗（続ける）: $*" >&2; }
  V=(-f lavfi -i "testsrc=size=96x64:rate=15" -f lavfi -i "sine=frequency=440:sample_rate=48000")
  T=(-t 0.6 -shortest)
  ff "${V[@]}" "${T[@]}" -c:v libx264 -g 4 -bf 2 -c:a aac -b:a 32k "$WORK/h264_aac.flv"
  ff "${V[@]}" "${T[@]}" -c:v libx265 -g 4 -x265-params log-level=none -an "$WORK/hevc.flv"
  ff "${V[@]}" "${T[@]}" -c:v libaom-av1 -cpu-used 8 -g 4 -an "$WORK/av1.flv"
  ff "${V[@]}" "${T[@]}" -c:v libx264 -g 4 -c:a aac -b:a 32k -f mpegts "$WORK/h264_aac.ts"
  ff "${V[@]}" "${T[@]}" -c:v libx265 -g 4 -x265-params log-level=none -c:a libopus -f mpegts "$WORK/hevc_opus.ts"
  ff "${V[@]}" "${T[@]}" -c:v libx264 -g 4 -c:a aac -b:a 32k -movflags frag_keyframe+empty_moov+default_base_moof "$WORK/frag.mp4"
  ff "${V[@]}" "${T[@]}" -c:v libvpx-vp9 -deadline realtime -g 4 -c:a libopus -movflags frag_keyframe+empty_moov "$WORK/vp9_opus.mp4"
  ff "${V[@]}" "${T[@]}" -c:v libaom-av1 -cpu-used 8 -g 4 -c:a libopus "$WORK/av1_opus.mp4"
  mkdir -p "$WORK/hls_fmp4" "$WORK/hls_ts"
  ff "${V[@]}" -t 2 -shortest -c:v libx264 -g 8 -c:a aac -b:a 32k -f hls -hls_time 0.5 -hls_segment_type fmp4 \
     -hls_flags independent_segments+program_date_time "$WORK/hls_fmp4/live.m3u8"
  ff "${V[@]}" -t 2 -shortest -c:v libx264 -g 8 -c:a aac -b:a 32k -f hls -hls_time 0.5 -hls_playlist_type vod \
     -master_pl_name master.m3u8 "$WORK/hls_ts/vod.m3u8"
  ff -f lavfi -i "testsrc=size=37x23" -frames:v 1 "$WORK/rgb.png"
  ff -f lavfi -i "testsrc=size=37x23" -frames:v 1 -pix_fmt gray16be "$WORK/gray16.png"
  ff -f lavfi -i "testsrc=size=37x23" -frames:v 1 -pix_fmt pal8 "$WORK/pal8.png"
  ff -f lavfi -i "testsrc=size=37x23" -frames:v 1 -pix_fmt yuvj420p "$WORK/yuv420.jpg"
  ff -f lavfi -i "testsrc=size=37x23" -frames:v 1 -pix_fmt yuvj422p "$WORK/yuv422.jpg"
  ff -f lavfi -i "testsrc=size=37x23" -frames:v 1 -pix_fmt gray "$WORK/gray.jpg"
  ff -f lavfi -i "sine=frequency=440:sample_rate=44100" -t 0.3 -c:a aac -b:a 32k -f adts "$WORK/sine.aac"
  ff -f lavfi -i "sine=frequency=440:sample_rate=48000" -t 0.3 -ac 2 -c:a libopus -frame_duration 2.5 "$WORK/short.opus"
  ff -f lavfi -i "testsrc=size=96x64:rate=15" -t 0.4 -c:v libaom-av1 -cpu-used 8 -g 2 -f obu "$WORK/ff.obu"
  ff -f lavfi -i "testsrc=size=96x64:rate=15" -t 0.4 -c:v libx264 -g 2 -bsf:v h264_mp4toannexb -f h264 "$WORK/ff.h264"
  ff -f lavfi -i "testsrc=size=96x64:rate=15" -t 0.4 -c:v libx265 -x265-params log-level=none -g 2 -f hevc "$WORK/ff.h265"
  shopt -s nullglob
  $PY codec "$CORPUS" "$WORK"/*.flv "$WORK"/*.ts "$WORK"/*.mp4 "$WORK"/hls_fmp4/* "$WORK"/hls_ts/*.m3u8 \
      "$WORK"/*.png "$WORK"/*.jpg "$WORK"/*.aac "$WORK"/*.h264 "$WORK"/*.h265
  # TS の HLS の最初のセグメント・Ogg・OBU
  $PY codec "$CORPUS" "$WORK"/hls_ts/vod0.ts
  $PY ogg "$CORPUS" "$WORK"/short.opus
  $PY adts "$CORPUS" "$WORK"/sine.aac
  [[ -f "$WORK/ff.obu" ]] && cp "$WORK/ff.obu" "$WORK/ff2.obu" && $PY codec "$CORPUS" "$WORK/ff.obu" && $PY av1rtp "$CORPUS" "$WORK/ff.obu"
  shopt -u nullglob
  echo "== FFmpeg の RTP"
  $PY rtp "$CORPUS" "$FFMPEG" "$WORK/rtp"
else
  echo "seed: FFmpeg がない（$FFMPEG）。FFmpeg 由来の種は作らない" >&2
fi

echo "== 自前の往復（seedgen）"
CARGO_TARGET_DIR="$CACHE/senca-seedgen-target" "$CARGO" build --quiet --manifest-path "$ROOT/tools/fuzz/seedgen/Cargo.toml"
"$CACHE/senca-seedgen-target/debug/senca-fuzz-seedgen" "$CORPUS" "$ROOT/crates/senca-tls/tests/data"

echo "== 手書き"
$PY text "$CORPUS"

echo "== target ごとの種の数"
for t in $(sed -n 's/^name = "\(.*\)"$/\1/p' "$ROOT/fuzz/Cargo.toml" | grep -v senca-fuzz); do
  n=$(find "$CORPUS/$t" -type f 2>/dev/null | wc -l)
  printf '%-14s %4d\n' "$t" "$n"
  if [[ "$n" == 0 ]]; then echo "seed: $t の種がない" >&2; fi
done
