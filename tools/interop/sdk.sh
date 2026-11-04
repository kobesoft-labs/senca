#!/usr/bin/env bash
# REQ-009: 送り手 SDK（senca-sdk）を SENCA のノードへ実際に送って確かめる。
#   A. Rust API → RTMP、B. Rust API → SRT（senca:// の設定文字列）、C. C の使用例（静的リンク）→ RTMP・SRT、
#   D. 断られたら Failed で止まる（鍵の誤り）、E. 暗号の SRT（AES-256）、F. QR コード（別実装の復号器で読み戻す）、
#   G. 送信中にノードを止めて起こし直す → SDK がつなぎ直してキーフレームから続ける。
# 届いた HLS のフレーム（復号した画の md5）が元素材の連続した一部と一致することを見る。
#
# 使い方: tools/interop/sdk.sh（KEEP=1 で作業場所を残す、ONLY=ABCDEFG で場面を絞る）
#   NODE_BIN: ノードの実行ファイル（既定 /tmp/senca-target-main/debug/senca-node、無ければ作る）
#   SDK_TARGET: SDK を作る場所（既定 ~/.cache/senca-target-sdk）
set -euo pipefail
HERE=$(cd "$(dirname "$0")" && pwd)
NODE_BIN=${NODE_BIN:-/tmp/senca-target-main/debug/senca-node}
source "$HERE/lib.sh"
SDK_TARGET=${SDK_TARGET:-$HOME/.cache/senca-target-sdk}
ONLY=${ONLY:-ABCDEFG}
CARGO=${CARGO:-$HOME/.cargo/bin/cargo}

# 作る（ノードは別の人も触っているので、壊れていたら少し待って作り直す）
for i in 1 2 3 4 5; do
  (cd "$ROOT" && CARGO_TARGET_DIR="$SDK_TARGET" "$CARGO" build -q -p senca-sdk --lib --examples) && break
  [ "$i" = 5 ] && { echo "sdk build failed"; exit 1; }
  echo "sdk build failed (try $i; the workspace may be mid-edit), retrying"; sleep 20
done
if [ ! -x "$NODE_BIN" ]; then
  for i in 1 2 3; do
    (cd "$ROOT" && CARGO_TARGET_DIR=/tmp/senca-target-main "$CARGO" build -q -p senca-node) && break
    echo "node build failed (try $i), retrying"; sleep 20
  done
fi
[ -x "$NODE_BIN" ] || { echo "no node binary: $NODE_BIN"; exit 1; }
EX=$SDK_TARGET/debug/examples

mkdir -p "$HOME/.cache"
W=$(mktemp -d -p "$HOME/.cache" senca-sdk-interop.XXXXXX); NODE_PID=
cleanup() { [ -n "$NODE_PID" ] && kill "$NODE_PID" 2>/dev/null || true; wait 2>/dev/null || true; [ -n "${KEEP:-}" ] && echo "kept $W" || rm -rf "$W"; }
trap cleanup EXIT

# C の使用例（静的リンクと共有ライブラリ）
make -s -C "$ROOT/crates/senca-sdk/examples/c" SDK_LIB="$SDK_TARGET/debug" OUT="$W" >/dev/null
"$W/sender" --version | grep -q "abi 1" && pass "C example builds (static) and reports ABI 1" || fail "C example version"
"$W/sender-shared" --version | grep -q "abi 1" && pass "C example builds (shared library)" || fail "C example shared"

GRACE=1000
HTTP=$(port); RTMP=$(port); SRT=$(port); API=http://127.0.0.1:$HTTP/api/v1
start_node() {
  "$NODE_BIN" --data-dir "$W/data" --http 127.0.0.1:$HTTP --rtmp 127.0.0.1:$RTMP --srt 127.0.0.1:$SRT \
    --public-rtmp-url rtmp://127.0.0.1:$RTMP --public-http-url http://127.0.0.1:$HTTP --reconnect-grace $GRACE >>"$W/node.log" 2>&1 &
  NODE_PID=$!
  for _ in $(seq 100); do curl -sf http://127.0.0.1:$HTTP/readyz >/dev/null && return; sleep 0.1; done
  echo "node did not start"; cat "$W/node.log"; exit 1
}
stop_node() { kill "$NODE_PID"; wait "$NODE_PID" 2>/dev/null || true; NODE_PID=; }
start_node
TOKEN=$(cat "$W/data/control/bootstrap-admin-token")
H=(-H "Authorization: Bearer $TOKEN")
api() { local m=$1 p=$2; shift 2; curl -s -X "$m" "${H[@]}" -H 'Content-Type: application/json' "$API$p" "$@"; }
new_stream() { api POST /streams -d "{\"name\":\"$1\",\"recording\":{\"enabled\":false},\"hls\":{\"window\":30}}" > "$W/$1.json"; }
key() { json "['ingest']['stream_key']" < "$W/$1.json"; }
sid() { json "['id']" < "$W/$1.json"; }
hls() { sleep $(( GRACE / 1000 + 1 )); api POST /streams/$(sid "$1")/playback-url | json "['playback']['hls']"; }
# 音声は復号せずパケット（AAC の raw_data_block）の md5 で比べる（途中から復号すると先頭の画が変わるため）
apkts() { { "$FFMPEG" -v error -i "$1" -map 0:a -c copy -f framemd5 - 2>/dev/null || true; } | { grep -v "^#" || true; } | awk -F, '{gsub(/ /,"",$6); print $6}'; }
# 名前 送った映像の数 元素材 説明 → 届いた映像（復号した画）と音声（パケット）が元素材の連続した一部であること。
# ノードの HLS は先頭の GOP（30 フレーム）を落とすことがある（node.sh と同じ許容）ので、映像は want-35 以上。
check() {
  local name=$1 want=$2 src=$3 url; url=$(hls "$name")
  curl -s "$url" > "$W/$name.m3u8"
  frames "$url" v > "$W/$name.v"; apkts "$url" > "$W/$name.ap"
  local nv na sa; nv=$(wc -l < "$W/$name.v"); na=$(wc -l < "$W/$name.ap"); sa=$(wc -l < "$src.ap")
  contiguous "$src.v" "$W/$name.v" $(( want - 35 )) && contiguous "$src.ap" "$W/$name.ap" $(( sa * 85 / 100 )) \
    && pass "$4 = contiguous source run (video $nv/$want, audio packets $na/$sa)" || fail "$4 mismatch (video $nv/$want, audio packets $na/$sa)"
}

# 素材: B フレームありの H.264 + AAC（6 秒）。C の例には AUD 付き・B フレームなしの Annex B と ADTS
"$FFMPEG" -v error -f lavfi -i testsrc2=size=640x360:rate=30 -f lavfi -i sine=frequency=440:sample_rate=48000 -t 6 \
  -c:v libx264 -g 30 -bf 2 -pix_fmt yuv420p -c:a aac -b:a 128k -f flv "$W/src.flv"
"$FFMPEG" -v error -i "$W/src.flv" -c copy -f mpegts "$W/src.ts"
frames "$W/src.flv" v > "$W/src.v"; apkts "$W/src.ts" > "$W/src.ap"
"$FFMPEG" -v error -f lavfi -i testsrc2=size=640x360:rate=30 -t 6 -c:v libx264 -g 30 -bf 0 -pix_fmt yuv420p \
  -bsf:v h264_metadata=aud=insert -f h264 "$W/c.h264"
"$FFMPEG" -v error -f lavfi -i sine=frequency=660:sample_rate=48000 -t 6 -c:a aac -b:a 96k -f adts "$W/c.aac"
frames "$W/c.h264" v > "$W/c.v"; apkts "$W/c.aac" > "$W/c.ap"
NV=$(wc -l < "$W/src.v"); NC=$(wc -l < "$W/c.v")

if [[ $ONLY == *A* ]]; then
  new_stream a
  "$EX/send_file" "rtmp://127.0.0.1:$RTMP/live/$(key a)" "$W/src.flv" > "$W/a.out" 2>"$W/a.err" \
    && pass "A Rust API RTMP send finished ($(cat "$W/a.out"))" || fail "A send_file RTMP failed: $(tail -2 "$W/a.err")"
  check a "$NV" "$W/src" "A Rust API → RTMP → HLS"
fi

if [[ $ONLY == *B* ]]; then
  new_stream b
  U="senca://ingest?v=1&proto=srt&host=127.0.0.1&port=$SRT&streamid=publish:$(key b)&latency=200"
  "$EX/send_file" "$U" "$W/src.ts" > "$W/b.out" 2>"$W/b.err" \
    && pass "B Rust API SRT send finished ($(cat "$W/b.out"))" || fail "B send_file SRT failed: $(tail -2 "$W/b.err")"
  grep -q "rtt_us=[0-9]" "$W/b.out" && pass "B SRT RTT measured" || fail "B no RTT"
  check b "$NV" "$W/src" "B Rust API → SRT (senca: URI) → HLS"
fi

if [[ $ONLY == *C* ]]; then
  new_stream c1
  "$W/sender" "rtmp://127.0.0.1:$RTMP/live/$(key c1)" "$W/c.h264" "$W/c.aac" 30 > "$W/c1.out" 2>"$W/c1.err" \
    && pass "C C example RTMP send finished ($(cat "$W/c1.out"))" || fail "C C example RTMP failed: $(tail -2 "$W/c1.err")"
  check c1 "$NC" "$W/c" "C C ABI (Annex B + ADTS) → RTMP → HLS"
  new_stream c2
  "$W/sender" "senca://ingest?v=1&proto=srt&host=127.0.0.1&port=$SRT&streamid=publish:$(key c2)" "$W/c.h264" "$W/c.aac" 30 \
    > "$W/c2.out" 2>"$W/c2.err" && pass "C C example SRT send finished ($(cat "$W/c2.out"))" || fail "C C example SRT failed: $(tail -2 "$W/c2.err")"
  check c2 "$NC" "$W/c" "C C ABI (Annex B + ADTS) → SRT → HLS"
fi

if [[ $ONLY == *D* ]]; then
  t0=$(date +%s)
  "$EX/send_file" "srt://127.0.0.1:$SRT?streamid=publish:sk_bogus" "$W/src.ts" > /dev/null 2>"$W/d1.err" && fail "D SRT bad key accepted" \
    || { grep -q "Failed" "$W/d1.err" && pass "D SRT bad key → Failed ($(grep -o 'srt: rejected[^"]*' "$W/d1.err" | head -1))" || fail "D SRT bad key: $(tail -1 "$W/d1.err")"; }
  "$EX/send_file" "rtmp://127.0.0.1:$RTMP/live/sk_bogus" "$W/src.flv" > /dev/null 2>"$W/d2.err" && fail "D RTMP bad key accepted" \
    || { grep -q "Failed" "$W/d2.err" && pass "D RTMP bad key → Failed (no retry)" || fail "D RTMP bad key: $(tail -1 "$W/d2.err")"; }
  [ $(( $(date +%s) - t0 )) -lt 15 ] && pass "D rejections are reported promptly" || fail "D rejection took too long"
  grep -q "sk_bogus" "$W/d1.err" "$W/d2.err" && fail "D stream key leaked in error output" || pass "D stream key not in error output"
fi

if [[ $ONLY == *E* ]]; then
  # テナントのパスフレーズを設定すると、平文の SRT は断られ、暗号の SRT だけ通る
  api POST /secrets -d '{"name":"srt-passphrase","value":"0123456789abcdef"}' >/dev/null
  new_stream e
  "$EX/send_file" "srt://127.0.0.1:$SRT?streamid=publish:$(key e)" "$W/src.ts" > /dev/null 2>"$W/e0.err" \
    && fail "E plaintext SRT accepted with passphrase required" || pass "E plaintext SRT refused when passphrase required"
  U="senca://ingest?v=1&proto=srt&host=127.0.0.1&port=$SRT&streamid=publish:$(key e)&passphrase=0123456789abcdef&pbkeylen=32"
  "$EX/send_file" "$U" "$W/src.ts" > "$W/e.out" 2>"$W/e.err" \
    && pass "E encrypted SRT send finished" || fail "E encrypted SRT failed: $(tail -2 "$W/e.err")"
  check e "$NV" "$W/src" "E Rust API → SRT AES-256 → HLS"
fi

if [[ $ONLY == *F* ]]; then
  D="python3 -I $HERE/qr_decode.py"
  # 規格の附属書の例（数字モード、型番 1-M）
  "$EX/qr" 01234567 "$W/q0.png" 4 > "$W/q0.txt"
  [ "$($D "$W/q0.png")" = 01234567 ] && grep -q "version=1 " "$W/q0.txt" && pass "F QR spec example 01234567 (1-M) decodes" || fail "F QR spec example"
  # 送り先の設定文字列（正規形にしてから符号化）を別実装の復号器で読み戻す
  for proto in srt rtmp whip; do
    case $proto in
      srt) U="senca://ingest?v=1&proto=srt&host=media.example.com&port=9000&streamid=publish:sk_0123456789abcdef0123&passphrase=correct-horse-battery&latency=200" ;;
      rtmp) U="rtmp://media.example.com/live/sk_0123456789abcdef0123" ;;
      whip) U="senca://ingest?v=1&proto=whip&url=https://media.example.com/whip/abc&token=eyJhbGciOiJFZERTQSJ9.e30.sig" ;;
    esac
    "$EX/qr" "$U" "$W/q-$proto.png" 4 > "$W/q-$proto.txt"
    got=$($D "$W/q-$proto.png") || got="(decode failed)"
    [ "$got" = "$U" ] && pass "F QR $proto config round-trips ($(cat "$W/q-$proto.txt"))" || fail "F QR $proto: got '$got'"
  done
  "$EX/qr" "$(python3 -c 'print("x"*213)')" "$W/q213.png" 2 | grep -q "version=10" && [ "$($D "$W/q213.png")" = "$(python3 -c 'print("x"*213)')" ] \
    && pass "F QR 213 bytes fits version 10-M" || fail "F QR capacity"
  "$EX/qr" "$(python3 -c 'print("x"*214)')" "$W/q214.png" 2>/dev/null && fail "F QR 214 bytes accepted" || pass "F QR 214 bytes rejected"
  "$EX/qr" "$U" "$W/q.svg" > /dev/null
  python3 -I -c "import sys,xml.etree.ElementTree as E;r=E.parse(sys.argv[1]).getroot();sys.exit(0 if r.tag.endswith('svg') and r.get('viewBox') else 1)" "$W/q.svg" \
    && pass "F QR SVG is well-formed XML" || fail "F QR SVG"
  "$EX/qr" "senca://ingest?v=1&proto=srt&host=h&port=0" "$W/bad.png" 2>/dev/null && fail "F invalid config accepted for QR" || pass "F invalid config refused for QR"
fi

if [[ $ONLY == *G* ]]; then
  # 12 秒の素材を RTMP で送り、4 秒目でノードを止め、1 秒後に起こし直す
  "$FFMPEG" -v error -f lavfi -i testsrc2=size=640x360:rate=30 -f lavfi -i sine=frequency=330:sample_rate=48000 -t 12 \
    -c:v libx264 -g 30 -bf 2 -pix_fmt yuv420p -c:a aac -b:a 128k -f flv "$W/g.flv"
  frames "$W/g.flv" v > "$W/g-src.v"
  new_stream g
  "$EX/send_file" "rtmp://127.0.0.1:$RTMP/live/$(key g)" "$W/g.flv" > "$W/g.out" 2>"$W/g.err" & SEND=$!
  sleep 4; stop_node; sleep 1; start_node
  # 止まっていた間のフレームは捨てる（ライブ優先）ので、送った数は入れた数より少ない
  wait $SEND || true
  grep -q "state=Stopped" "$W/g.out" && pass "G send finished across node restart ($(cat "$W/g.out"))" || fail "G send failed: $(tail -2 "$W/g.err")"
  grep -q "reconnects=[1-9]" "$W/g.out" && grep -q "Reconnecting" "$W/g.err" && pass "G SDK reconnected with backoff" || fail "G no reconnect"
  grep -q "KeyframeRequested" "$W/g.err" && pass "G keyframe requested after reconnect" || fail "G no keyframe request"
  URL=$(hls g); frames "$URL" v > "$W/g.v"
  # 起こし直した後の分が元素材の連続した一部（キーフレームから）
  contiguous "$W/g-src.v" "$W/g.v" 1 && pass "G HLS after restart = contiguous source run ($(wc -l < "$W/g.v") frames)" \
    || { python3 - "$W/g-src.v" "$W/g.v" <<'PY' && pass "G HLS after restart: last $(wc -l < "$W/g.v") frames include a contiguous run ≥150 from source" || fail "G HLS after restart mismatch ($(wc -l < "$W/g.v"))"
import sys
s=open(sys.argv[1]).read().split(); p=open(sys.argv[2]).read().split()
best=0
for i in range(len(p)):
    for j in range(len(s)):
        k=0
        while i+k<len(p) and j+k<len(s) and p[i+k]==s[j+k]: k+=1
        best=max(best,k)
    if best>=150: break
sys.exit(0 if best>=150 else 1)
PY
    }
fi

grep -q "sk_" "$W/node.log" && fail "stream key leaked to node log" || pass "no stream key in node log"
summary
