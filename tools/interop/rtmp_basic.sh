#!/usr/bin/env bash
# RTMP の相互接続試験（FFmpeg を相手にする）。
#
#  A. FFmpeg が H.264/AAC を直接 publish → senca の試験サーバーが FLV に記録 → フレーム数・コーデックを確認
#  B. FFmpeg が source.flv をそのまま publish → 記録した FLV と source.flv の復号結果（framemd5）が一致
#  C. senca の試験サーバーが source.flv を play で配信 → FFmpeg の受信結果（framemd5）が一致
#  D. senca の ClientSession が FFmpeg の待ち受け（-listen 1）へ publish → FFmpeg の記録が一致
#
# 環境変数: FFMPEG・FFPROBE（既定 ~/tools/ffmpeg・~/tools/ffprobe）、CARGO（既定 cargo）、
#           RTMP_BIN_DIR（指定すればビルドせず、そこにある rtmp_server・rtmp_push を使う）。
set -euo pipefail

FFMPEG=${FFMPEG:-$HOME/tools/ffmpeg}
FFPROBE=${FFPROBE:-$HOME/tools/ffprobe}
CARGO=${CARGO:-cargo}
ROOT=$(cd "$(dirname "$0")/../.." && pwd)

WORK=$(mktemp -d)
PIDS=()
cleanup() {
    for p in "${PIDS[@]}"; do kill "$p" 2>/dev/null || true; done
    rm -rf "$WORK"
}
trap cleanup EXIT

fail() {
    echo "FAIL: $*"
    exit 1
}

if [[ -z "${RTMP_BIN_DIR:-}" ]]; then
    (cd "$ROOT" && "$CARGO" build -q -p senca-rtmp --examples)
    RTMP_BIN_DIR="${CARGO_TARGET_DIR:-$ROOT/target}/debug/examples"
fi
SERVER="$RTMP_BIN_DIR/rtmp_server"
PUSH="$RTMP_BIN_DIR/rtmp_push"
[[ -x "$SERVER" && -x "$PUSH" ]] || fail "examples not found in $RTMP_BIN_DIR"

BASE=$((41000 + RANDOM % 900))
FF=("$FFMPEG" -hide_banner -loglevel error -nostdin)

# 試験サーバーを起動し、LISTENING が出るまで待つ。
start_server() { # mode port file log
    "$SERVER" "$1" "$2" "$3" >"$4.out" 2>"$4.err" &
    PIDS+=($!)
    LAST_PID=$!
    for _ in $(seq 100); do
        grep -q LISTENING "$4.out" 2>/dev/null && return 0
        sleep 0.05
    done
    fail "server did not start"
}

wait_listen() { # port
    for _ in $(seq 200); do
        ss -ltn 2>/dev/null | grep -q ":$1 " && return 0
        sleep 0.05
    done
    fail "port $1 not listening"
}

framemd5() { # input output
    "${FF[@]}" -i "$1" -map 0 -f framemd5 - | grep -v '^#' >"$2"
}

frames() { # file stream
    "$FFPROBE" -v error -select_streams "$2" -count_frames -show_entries stream=nb_read_frames -of csv=p=0 "$1"
}

stream_info() { # file stream entries
    "$FFPROBE" -v error -select_streams "$2" -show_entries stream="$3" -of csv=p=0 "$1"
}

cd "$WORK"

echo "== source"
"${FF[@]}" -f lavfi -i testsrc2=size=320x240:rate=30 -f lavfi -i sine -t 5 \
    -c:v libx264 -g 30 -bf 2 -c:a aac -f flv source.flv
framemd5 source.flv source.md5

echo "== A: ffmpeg publish (encode) -> senca record"
P=$BASE
start_server record "$P" a.flv a
"${FF[@]}" -re -f lavfi -i testsrc2=size=320x240:rate=30 -f lavfi -i sine -t 5 \
    -c:v libx264 -g 30 -bf 2 -c:a aac -f flv "rtmp://127.0.0.1:$P/live/key"
wait "$LAST_PID" || fail "A: server exited with error: $(cat a.err)"
[[ "$(frames a.flv v:0)" == "150" ]] || fail "A: video frame count $(frames a.flv v:0) != 150"
[[ "$(stream_info a.flv v:0 codec_name,width,height)" == "h264,320,240" ]] ||
    fail "A: video params $(stream_info a.flv v:0 codec_name,width,height)"
[[ "$(stream_info a.flv a:0 codec_name,sample_rate,channels)" == "$(stream_info source.flv a:0 codec_name,sample_rate,channels)" ]] ||
    fail "A: audio params differ"
[[ "$(frames a.flv a:0)" == "$(frames source.flv a:0)" ]] ||
    fail "A: audio frame count $(frames a.flv a:0) != $(frames source.flv a:0)"
"${FF[@]}" -i a.flv -f null - || fail "A: decode error"
echo "A: PASS ($(cat a.err | tail -1))"

echo "== B: ffmpeg publish (copy source.flv) -> senca record, framemd5"
P=$((BASE + 1))
start_server record "$P" b.flv b
"${FF[@]}" -re -i source.flv -c copy -f flv "rtmp://127.0.0.1:$P/live/key"
wait "$LAST_PID" || fail "B: server exited with error: $(cat b.err)"
framemd5 b.flv b.md5
cmp -s source.md5 b.md5 || { diff source.md5 b.md5 | head; fail "B: framemd5 differs"; }
echo "B: PASS ($(wc -l <b.md5) frames)"

echo "== C: senca serve (play) -> ffmpeg, framemd5"
P=$((BASE + 2))
start_server serve "$P" source.flv c
"${FF[@]}" -i "rtmp://127.0.0.1:$P/live/key" -map 0 -f framemd5 - | grep -v '^#' >c.md5
wait "$LAST_PID" || fail "C: server exited with error: $(cat c.err)"
cmp -s source.md5 c.md5 || { diff source.md5 c.md5 | head; fail "C: framemd5 differs"; }
echo "C: PASS ($(wc -l <c.md5) frames)"

echo "== D: senca push (ClientSession) -> ffmpeg -listen 1, framemd5"
P=$((BASE + 3))
"${FF[@]}" -listen 1 -i "rtmp://127.0.0.1:$P/live/key" -c copy -f flv d.flv &
FFPID=$!
PIDS+=("$FFPID")
wait_listen "$P"
"$PUSH" "127.0.0.1:$P" live key source.flv 2>d.err || fail "D: push failed: $(cat d.err)"
wait "$FFPID" || fail "D: ffmpeg listener failed"
framemd5 d.flv d.md5
cmp -s source.md5 d.md5 || { diff source.md5 d.md5 | head; fail "D: framemd5 differs"; }
echo "D: PASS ($(wc -l <d.md5) frames)"

echo "PASS"
