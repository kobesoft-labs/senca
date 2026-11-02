#!/usr/bin/env bash
# M1〜M3: RTMP → ライブ HLS・録画・VOD・MP4 書き出し・回収・RTMP play・API の契約。
# 使い方: tools/interop/node.sh（KEEP=1 で作業場所を残す、ONLY=ABC で場面を絞る）
source "$(dirname "$0")/lib.sh"
[ -n "${NODE_BIN_PREBUILT:-}" ] || (cd "$ROOT" && cargo build -q -p senca-node -p senca-codec-worker -p senca-devtools && cargo build -q -p senca-rtmp --examples)
# ロイヤリティフリーのコーデック（dav1d など）の置き場所。無ければ G を飛ばす
CODEC_LIB_DIR=${CODEC_LIB_DIR:-$HOME/tools/codeclibs/usr/lib/x86_64-linux-gnu}
W=$(mktemp -d); NODE_PID=
cleanup() { [ -n "$NODE_PID" ] && kill "$NODE_PID" 2>/dev/null || true; wait 2>/dev/null || true; [ -n "${KEEP:-}" ] && echo "kept $W" || rm -rf "$W"; }
trap cleanup EXIT
ONLY=${ONLY:-ABCDEFG}
HTTP=$(port); RTMP=$(port); API=http://127.0.0.1:$HTTP/api/v1
start_node() {
  "$NODE_BIN" --data-dir "$W/data" --http 127.0.0.1:$HTTP --rtmp 127.0.0.1:$RTMP \
    --public-rtmp-url rtmp://127.0.0.1:$RTMP --public-http-url http://127.0.0.1:$HTTP --reconnect-grace "${GRACE:-3000}" \
    --codec-lib-dir "$CODEC_LIB_DIR" --codec-workers 2 "$@" >>"$W/node.log" 2>&1 &
  NODE_PID=$!
  for _ in $(seq 50); do curl -sf http://127.0.0.1:$HTTP/readyz >/dev/null && return; sleep 0.1; done
  echo "node did not start"; cat "$W/node.log"; exit 1
}
stop_node() { kill -${1:-TERM} "$NODE_PID"; wait "$NODE_PID" 2>/dev/null || true; NODE_PID=; }
start_node
TOKEN=$(cat "$W/data/control/bootstrap-admin-token")
H=(-H "Authorization: Bearer $TOKEN")
api() { local m=$1 p=$2; shift 2; curl -s -X "$m" "${H[@]}" -H 'Content-Type: application/json' "$API$p" "$@"; }

"$FFMPEG" -v error -f lavfi -i testsrc2=size=640x360:rate=30 -f lavfi -i sine=frequency=440:sample_rate=48000 -t 12 \
  -c:v libx264 -g 30 -bf 2 -pix_fmt yuv420p -c:a aac -b:a 128k -f flv "$W/src.flv"
frames "$W/src.flv" v > "$W/src.v"; frames "$W/src.flv" a > "$W/src.a"

new_stream() { api POST /streams -d "{\"name\":\"$1\",\"recording\":{\"enabled\":${2:-true}}}" > "$W/$1.json"; }
key() { json "['ingest']['stream_key']" < "$W/$1.json"; }
sid() { json "['id']" < "$W/$1.json"; }
latest_rec() { api GET "/recordings?stream=$1" | python3 -c "import json,sys;i=json.load(sys.stdin)['items'];print(sorted(i,key=lambda r:r['started_at'])[-1]['id'])"; }
wait_state() { for _ in $(seq 100); do s=$(api GET /recordings/$1 | json "['state']"); [ "$s" = "$2" ] && return 0; sleep 0.1; done; return 1; }
publish() { "$FFMPEG" -v error -re -i "${2:-$W/src.flv}" -c copy -f flv rtmp://127.0.0.1:$RTMP/live/$1; }

if [[ $ONLY == *A* ]]; then
  # A. ライブ HLS と録画（VOD）と MP4 書き出しが元素材とビット一致
  new_stream a; publish "$(key a)"; sleep $(( ${GRACE:-3000} / 1000 + 1 ))
  URL=$(api POST /streams/$(sid a)/playback-url | json "['playback']['hls']")
  frames "$URL" v > "$W/live.v"; frames "$URL" a > "$W/live.a"
  cmp -s "$W/src.v" "$W/live.v" && cmp -s "$W/src.a" "$W/live.a" && pass "A live HLS bit-identical ($(wc -l <"$W/live.v") video frames)" || fail "A live HLS differs"
  R=$(latest_rec "$(sid a)"); wait_state "$R" finalized || fail "A recording not finalized"
  V=$(api POST /recordings/$R/playback-url | json "['playback']['hls']")
  frames "$V" v > "$W/vod.v"; frames "$V" a > "$W/vod.a"
  cmp -s "$W/src.v" "$W/vod.v" && cmp -s "$W/src.a" "$W/vod.a" && pass "A VOD bit-identical" || fail "A VOD differs"
  curl -s "${H[@]}" -o "$W/exp.mp4" "$API/recordings/$R/export.mp4"
  frames "$W/exp.mp4" v > "$W/exp.v"; frames "$W/exp.mp4" a > "$W/exp.a"
  cmp -s "$W/src.v" "$W/exp.v" && cmp -s "$W/src.a" "$W/exp.a" && pass "A MP4 export bit-identical" || fail "A MP4 export differs"
  "$FFPROBE" -v error -show_entries format=format_name -of csv=p=0 "$W/exp.mp4" | grep -q mp4 && pass "A MP4 probes as mp4" || fail "A MP4 probe"
fi

if [[ $ONLY == *B* ]]; then
  # B. 猶予内の再接続: 1 つの録画、不連続 1 つ、epoch ごとに元素材と一致
  new_stream b; publish "$(key b)"; sleep 0.5; publish "$(key b)"
  R=$(latest_rec "$(sid b)"); wait_state "$R" finalized || fail "B not finalized"
  n=$(api GET "/recordings?stream=$(sid b)" | json "['items'].__len__()")
  [ "$n" = 1 ] && pass "B one recording across reconnect" || fail "B expected 1 recording, got $n"
  V=$(api POST /recordings/$R/playback-url | json "['playback']['hls']")
  curl -s "$V" > "$W/b.m3u8"
  [ "$(grep -c EXT-X-DISCONTINUITY "$W/b.m3u8")" = 1 ] && pass "B one DISCONTINUITY" || fail "B discontinuity count"
  # 区間ごとに比べる（ffmpeg は不連続をまたいで音声の状態を引き継ぐため）
  ok=1; for part in 1 2; do
    awk -v want=$part 'BEGIN{p=1} /EXT-X-DISCONTINUITY/{p++} p==want && /\.ts$/{print}' "$W/b.m3u8" > "$W/b$part.list"
    : > "$W/b$part.ts"; while read -r f; do curl -s "${V%index.m3u8}$f" >> "$W/b$part.ts"; done < "$W/b$part.list"
    frames "$W/b$part.ts" v > "$W/b$part.v"; cmp -s "$W/src.v" "$W/b$part.v" || ok=0
  done
  [ $ok = 1 ] && pass "B each epoch bit-identical" || fail "B epoch mismatch"
fi

if [[ $ONLY == *C* ]]; then
  # C. 録画中に kill -9 → 再起動: interrupted で回収、残った分は元素材の先頭と一致
  new_stream c; publish "$(key c)" & PUB=$!; sleep 7
  R=$(latest_rec "$(sid c)")
  stop_node KILL; wait $PUB 2>/dev/null || true
  start_node
  s=$(api GET /recordings/$R | json "['state']")
  [ "$s" = interrupted ] && pass "C recovered as interrupted" || fail "C state=$s"
  V=$(api POST /recordings/$R/playback-url | json "['playback']['hls']")
  frames "$V" v > "$W/c.v"; n=$(wc -l < "$W/c.v")
  [ "$n" -gt 0 ] && head -n "$n" "$W/src.v" | cmp -s - "$W/c.v" && pass "C recovered $n frames = source prefix" || fail "C recovered content mismatch ($n)"
  ls "$W"/data/segments/*/*/.*.tmp >/dev/null 2>&1 && fail "C temp files left" || pass "C no temp files"
fi

if [[ $ONLY == *D* ]]; then
  # D. RTMP play（署名付き）: 送り手の途中から入っても、キーフレームから元素材と一致
  new_stream d false
  publish "$(key d)" & PUB=$!; sleep 2
  P=$(api POST /streams/$(sid d)/playback-url | json "['playback']['rtmp']")
  "$FFMPEG" -v error -i "$P" -map 0:v -f framemd5 - 2>/dev/null | grep -v '^#' | awk -F, '{gsub(/ /,"",$6);print $6}' > "$W/play.v" || true
  wait $PUB || true
  n=$(wc -l < "$W/play.v")
  python3 - "$W/src.v" "$W/play.v" <<'PY' && pass "D RTMP play $n frames = contiguous source run" || fail "D RTMP play mismatch ($n)"
import sys
s=open(sys.argv[1]).read().split(); p=open(sys.argv[2]).read().split()
sys.exit(0 if p and any(s[i:i+len(p)]==p for i in range(len(s)-len(p)+1)) else 1)
PY
  "$FFMPEG" -v error -i "rtmp://127.0.0.1:$RTMP/play/$(sid d)?exp=1&sig=AAAA" -t 1 -f null - 2>/dev/null && fail "D bad signature accepted" || pass "D bad signature rejected"
fi

if [[ $ONLY == *E* ]]; then
  # E. API の契約
  c=$(curl -s -o /dev/null -w '%{http_code}' "$API/streams"); [ "$c" = 401 ] && pass "E no token 401" || fail "E no token $c"
  c=$(curl -s -o /dev/null -w '%{http_code}' -H 'Authorization: Bearer senca_bogus' "$API/streams"); [ "$c" = 401 ] && pass "E bad token 401" || fail "E bad token $c"
  r1=$(api POST /streams -H 'Idempotency-Key: k1' -d '{"name":"idem"}'); r2=$(api POST /streams -H 'Idempotency-Key: k1' -d '{"name":"idem"}')
  [ "$(echo "$r1" | json "['id']")" = "$(echo "$r2" | json "['id']")" ] && pass "E idempotent replay same id" || fail "E idempotency"
  echo "$r2" | json "['ingest']['stream_key']" | grep -q None && pass "E replay hides stream key" || fail "E replay leaked key"
  c=$(api POST /streams -H 'Idempotency-Key: k1' -d '{"name":"other"}' -o /dev/null -w '%{http_code}'); [ "$c" = 409 ] && pass "E same key different body 409" || fail "E idem conflict $c"
  ID=$(echo "$r1" | json "['id']")
  c=$(api PATCH /streams/$ID -d '{"name":"x"}' -o /dev/null -w '%{http_code}'); [ "$c" = 428 ] && pass "E PATCH without If-Match 428" || fail "E 428 $c"
  c=$(api PATCH /streams/$ID -H 'If-Match: "9"' -d '{"name":"x"}' -o /dev/null -w '%{http_code}'); [ "$c" = 412 ] && pass "E stale If-Match 412" || fail "E 412 $c"
  c=$(api PATCH /streams/$ID -H 'If-Match: "1"' -d '{"name":"x"}' -o /dev/null -w '%{http_code}'); [ "$c" = 200 ] && pass "E PATCH ok" || fail "E patch $c"
  c=$(api POST /streams -d '{"name":"y","bogus":1}' -o /dev/null -w '%{http_code}'); [ "$c" = 400 ] && pass "E unknown field 400" || fail "E unknown field $c"
  c=$(curl -s -o /dev/null -w '%{http_code}' "${H[@]}" -H 'Content-Type: text/plain' -d x "$API/streams"); [ "$c" = 415 ] && pass "E wrong content type 415" || fail "E 415 $c"
  # 制限付きトークン
  T2=$(api POST /tokens -d '{"name":"ro","scopes":["stream:read"]}' | json "['token']")
  c=$(curl -s -o /dev/null -w '%{http_code}' -H "Authorization: Bearer $T2" -H 'Content-Type: application/json' -d '{"name":"z"}' "$API/streams"); [ "$c" = 403 ] && pass "E read-only token cannot create 403" || fail "E scope $c"
  grep -q "sk_" "$W/node.log" "$W/data/audit.log" "$W/data/control/log" 2>/dev/null && fail "E stream key leaked to log/state" || pass "E no stream key in logs/state"
fi

if [[ $ONLY == *F* ]]; then
  # F. 停止の SIGTERM で録画が finalized で確定
  new_stream f; publish "$(key f)" & PUB=$!; sleep 5
  R=$(latest_rec "$(sid f)"); stop_node TERM; wait $PUB 2>/dev/null || true
  grep -q '"t":"end".*finalized' "$W/data/recordings/$R/index.jsonl" && pass "F SIGTERM finalizes recording" || fail "F not finalized"
  start_node
fi

if [[ $ONLY == *G* ]]; then
  # G. ロイヤリティフリー（AV1＋Opus、Enhanced RTMP、fMP4）: ライブ・今の画・録画・サムネイル・書き出し。
  #    H.264 の今の画はハードウェアが無いので理由付きの 503。
  if [ ! -e "$CODEC_LIB_DIR/libdav1d.so.7" ]; then echo "SKIP G (no $CODEC_LIB_DIR/libdav1d.so.7)"; else
  "$FFMPEG" -v error -f lavfi -i testsrc2=size=640x360:rate=30 -f lavfi -i sine=sample_rate=48000 -t 6 \
    -c:v libaom-av1 -cpu-used 8 -row-mt 1 -g 30 -b:v 500k -c:a libopus "$W/av1.mp4"
  "$TARGET/debug/mp4-to-flv" "$W/av1.mp4" "$W/av1.flv"
  api POST /streams -d '{"name":"g","codec_profile":"royalty_free","hls":{"container":"fmp4"},"recording":{"enabled":true}}' > "$W/g.json"
  "$TARGET/debug/examples/rtmp_push" 127.0.0.1:$RTMP live "$(key g)" "$W/av1.flv" >/dev/null 2>&1
  c=$(curl -s -o "$W/snap.jpg" -w '%{http_code}' "${H[@]}" "$API/streams/$(sid g)/snapshot")
  [ "$c" = 200 ] && "$FFPROBE" -v error -show_entries stream=width,height -of csv=p=0 "$W/snap.jpg" | grep -q '640,360' && pass "G AV1 snapshot JPEG 640x360" || fail "G snapshot $c"
  sleep $(( ${GRACE:-3000} / 1000 + 1 ))
  URL=$(api POST /streams/$(sid g)/playback-url | json "['playback']['hls']")
  frames "$W/av1.mp4" v > "$W/g.sv"; frames "$URL" v > "$W/g.lv"
  cmp -s "$W/g.sv" "$W/g.lv" && pass "G live fMP4 AV1 bit-identical" || fail "G live AV1 differs"
  # 最初の映像キーフレームより前の音声 1 パケットは捨てるので、Opus の復号器が落ち着いた後で比べる
  frames "$W/av1.mp4" a | tail -n 200 > "$W/g.sa"; frames "$URL" a | tail -n 200 > "$W/g.la"
  cmp -s "$W/g.sa" "$W/g.la" && pass "G live Opus identical (last 200 frames)" || fail "G live Opus differs"
  R=$(latest_rec "$(sid g)"); wait_state "$R" finalized || fail "G not finalized"
  for _ in $(seq 50); do [ "$(api GET /recordings/$R | json "['thumbnail']")" = True ] && break; sleep 0.2; done
  [ "$(api GET /recordings/$R | json "['thumbnail']")" = True ] && pass "G recording thumbnail generated" || fail "G no thumbnail"
  curl -s "${H[@]}" -o "$W/g.mp4" "$API/recordings/$R/export.mp4"; frames "$W/g.mp4" v > "$W/g.ev"
  cmp -s "$W/g.sv" "$W/g.ev" && pass "G export AV1 bit-identical" || fail "G export differs"
  # H.264 は GPU が無いと復号しない
  new_stream gh false; publish "$(key gh)" & PUB=$!; sleep 2
  c=$(curl -s -o "$W/gh.json" -w '%{http_code}' "${H[@]}" "$API/streams/$(sid gh)/snapshot"); wait $PUB || true
  [ "$c" = 503 ] && grep -q CAPACITY_UNAVAILABLE "$W/gh.json" && pass "G H.264 snapshot without GPU is 503" || fail "G H.264 snapshot $c"
  fi
fi
summary
