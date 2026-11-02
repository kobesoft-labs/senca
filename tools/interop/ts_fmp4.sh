#!/usr/bin/env bash
# senca-mpegts / senca-fmp4 と FFmpeg の突き合わせ。
#
# FFmpeg で作った入力を SENCA の分離器・多重化器・書き手に通し、FFmpeg で復号したフレームが
# 元の復号と一致すること（framemd5）、ffprobe がエラーを出さないこと、時刻が一致することを確かめる。
#
#   - TS → TS（ts_remux）: framemd5 を時刻ごと完全一致で比べる（同じ 90 kHz の時間軸）。
#   - TS → fMP4 / MP4（ts_to_mp4）、MP4 → fMP4 / MP4（mp4_rewrite）: 時間軸が変わるので、
#     各ファイルで最も早い PTS を 0 として ms に丸めた時刻とハッシュを比べる。
#
# 環境変数: FFMPEG・FFPROBE（既定 ~/tools/ffmpeg・~/tools/ffprobe）、
#           BIN_DIR（例のバイナリの場所。指定しなければ cargo build する）。
set -euo pipefail

FFMPEG=${FFMPEG:-$HOME/tools/ffmpeg}
FFPROBE=${FFPROBE:-$HOME/tools/ffprobe}
ROOT=$(cd "$(dirname "$0")/../.." && pwd)

WORK=$(mktemp -d)
cleanup() { rm -rf "$WORK"; }
trap cleanup EXIT

if [[ -z "${BIN_DIR:-}" ]]; then
  CARGO=${CARGO:-cargo}
  command -v "$CARGO" >/dev/null || CARGO=$HOME/.cargo/bin/cargo
  (cd "$ROOT" && "$CARGO" build -q -p senca-mpegts -p senca-fmp4 --examples)
  BIN_DIR=${CARGO_TARGET_DIR:-$ROOT/target}/debug/examples
fi

PASS=0
FAIL=0
pass() { echo "PASS $1"; PASS=$((PASS + 1)); }
fail() { echo "FAIL $1: $2"; FAIL=$((FAIL + 1)); }

# この静的ビルドの FFmpeg は SDT（PID 0x11）の文字コード変換で落ちるので、入力から SDT を除く。
strip_sdt() {
  python3 -I -c '
import sys
d = open(sys.argv[1], "rb").read()
out = [d[i:i+188] for i in range(0, len(d) - 187, 188) if ((d[i+1] & 0x1F) << 8 | d[i+2]) != 0x11]
open(sys.argv[2], "wb").write(b"".join(out))
' "$1" "$2"
}

# framemd5 を「ストリーム 時刻(ms, 最小 PTS 基準) ハッシュ」に直して並べる。
cat > "$WORK/norm.awk" <<'AWK'
/^#tb/ { split($3, a, "/"); tb[substr($2, 1, length($2) - 1)] = a[1] / a[2]; next }
/^#/ { next }
{
  gsub(/ /, ""); split($0, f, ",")
  s = f[1]; t = f[3] * tb[s]
  st[NR] = s; ts[NR] = t; h[NR] = f[6]
  if (min == "" || t < min) min = t
}
END { for (i in st) printf "%s %d %s\n", st[i], int((ts[i] - min) * 1000 + 0.5), h[i] }
AWK

# 復号して framemd5 を書く。stderr に何か出たら失敗。
decode() {
  local in=$1 out=$2
  "$FFMPEG" -nostdin -v error -i "$in" -f framemd5 - >"$out" 2>"$out.err" || return 1
  [[ ! -s "$out.err" ]] || { cat "$out.err" >&2; return 1; }
}

probe_clean() {
  local err
  err=$("$FFPROBE" -v error -show_entries stream=codec_name -of csv "$1" 2>&1 >/dev/null) || return 1
  [[ -z "$err" ]] || { echo "$err" >&2; return 1; }
}

# $1 名前 $2 元 $3 出力 $4 exact|norm
compare() {
  local name=$1 src=$2 out=$3 mode=$4
  if ! probe_clean "$out"; then fail "$name" "ffprobe reported errors"; return; fi
  if ! decode "$src" "$WORK/a.md5"; then fail "$name" "decode of source failed"; return; fi
  if ! decode "$out" "$WORK/b.md5"; then fail "$name" "decode of output failed"; return; fi
  local frames
  frames=$(grep -vc '^#' "$WORK/a.md5" || true)
  if [[ "$frames" -lt 10 ]]; then fail "$name" "too few frames ($frames)"; return; fi
  if [[ $mode == exact ]]; then
    if ! diff -q <(grep -v '^#software' "$WORK/a.md5") <(grep -v '^#software' "$WORK/b.md5") >/dev/null; then
      fail "$name" "framemd5 differs"; diff <(grep -v '^#' "$WORK/a.md5") <(grep -v '^#' "$WORK/b.md5") | head -5; return
    fi
  else
    if ! diff -q <(awk -f "$WORK/norm.awk" "$WORK/a.md5" | sort) <(awk -f "$WORK/norm.awk" "$WORK/b.md5" | sort) >/dev/null; then
      fail "$name" "decoded frames or timestamps differ"
      diff <(awk -f "$WORK/norm.awk" "$WORK/a.md5" | sort) <(awk -f "$WORK/norm.awk" "$WORK/b.md5" | sort) | head -5
      return
    fi
  fi
  pass "$name ($frames frames)"
}

SRC_V=(-f lavfi -i testsrc2=size=320x240:rate=25)
SRC_A=(-f lavfi -i sine=frequency=440:sample_rate=48000)

gen() { "$FFMPEG" -nostdin -v error -y "${SRC_V[@]}" "${SRC_A[@]}" -t 3 "$@"; }

X264=(-c:v libx264 -bf 2 -g 25 -pix_fmt yuv420p)
X265=(-c:v libx265 -x265-params log-level=error -bf 2 -g 25 -pix_fmt yuv420p)

# --- TS 入力 ---
gen "${X264[@]}" -c:a aac -ac 2 -f mpegts "$WORK/h264_aac.raw.ts"
gen "${X265[@]}" -c:a aac -ac 2 -f mpegts "$WORK/h265_aac.raw.ts"
gen "${X264[@]}" -c:a libopus -ac 2 -f mpegts "$WORK/h264_opus.raw.ts"
gen "${X265[@]}" -c:a libopus -ac 2 -f mpegts "$WORK/h265_opus.raw.ts"

for c in h264_aac h265_aac h264_opus h265_opus; do
  strip_sdt "$WORK/$c.raw.ts" "$WORK/$c.ts"
  if "$BIN_DIR/ts_remux" "$WORK/$c.ts" "$WORK/$c.remux.ts" 2>"$WORK/log"; then
    compare "ts->ts $c" "$WORK/$c.ts" "$WORK/$c.remux.ts" exact
  else
    fail "ts->ts $c" "$(tail -1 "$WORK/log")"
  fi
  for m in frag progressive; do
    if "$BIN_DIR/ts_to_mp4" --$m "$WORK/$c.ts" "$WORK/$c.$m.mp4" 2>"$WORK/log"; then
      compare "ts->mp4($m) $c" "$WORK/$c.ts" "$WORK/$c.$m.mp4" norm
    else
      fail "ts->mp4($m) $c" "$(tail -1 "$WORK/log")"
    fi
  done
done

# --- fMP4 入力（AV1・VP9 は TS に載せないのでこちらで確かめる）---
FRAG=(-movflags frag_keyframe+default_base_moof)
gen -c:v libaom-av1 -cpu-used 8 -g 25 -pix_fmt yuv420p -c:a libopus "${FRAG[@]}" "$WORK/av1_opus.mp4"
gen -c:v libvpx-vp9 -deadline realtime -g 25 -pix_fmt yuv420p -c:a aac "${FRAG[@]}" "$WORK/vp9_aac.mp4"
gen "${X264[@]}" -c:a aac -movflags frag_keyframe+empty_moov "$WORK/h264_aac_frag.mp4"
gen "${X265[@]}" -tag:v hvc1 -c:a libopus "${FRAG[@]}" "$WORK/h265_opus_frag.mp4"

for c in av1_opus vp9_aac h264_aac_frag h265_opus_frag; do
  for m in frag progressive; do
    if "$BIN_DIR/mp4_rewrite" --$m "$WORK/$c.mp4" "$WORK/$c.out.$m.mp4" 2>"$WORK/log"; then
      compare "mp4->mp4($m) $c" "$WORK/$c.mp4" "$WORK/$c.out.$m.mp4" norm
    else
      fail "mp4->mp4($m) $c" "$(tail -1 "$WORK/log")"
    fi
  done
done

echo "passed: $PASS, failed: $FAIL"
[[ $FAIL -eq 0 ]]
