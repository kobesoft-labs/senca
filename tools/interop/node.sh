#!/usr/bin/env bash
# M1〜M3: RTMP → ライブ HLS・録画・VOD・MP4 書き出し・回収・RTMP play・API の契約。
# 使い方: tools/interop/node.sh（KEEP=1 で作業場所を残す、ONLY=ABC で場面を絞る）
source "$(dirname "$0")/lib.sh"
[ -n "${NODE_BIN_PREBUILT:-}" ] || (cd "$ROOT" && cargo build -q -p senca-node -p senca-codec-worker -p senca-capture -p senca-devtools && cargo build -q -p senca-rtmp -p senca-paas --examples)
# ロイヤリティフリーのコーデック（dav1d など）の置き場所。無ければ G を飛ばす
CODEC_LIB_DIR=${CODEC_LIB_DIR:-$HOME/tools/codeclibs/usr/lib/x86_64-linux-gnu}
W=$(mktemp -d); NODE_PID=
cleanup() { [ -n "$NODE_PID" ] && kill "$NODE_PID" 2>/dev/null || true; wait 2>/dev/null || true; [ -n "${KEEP:-}" ] && echo "kept $W" || rm -rf "$W"; }
trap cleanup EXIT
ONLY=${ONLY:-ABCDEFGHIJKLMNO}
HTTP=$(port); RTMP=$(port); SRT=$(port); RTSP=$(port); API=http://127.0.0.1:$HTTP/api/v1
start_node() {
  "$NODE_BIN" --data-dir "$W/data" --http 127.0.0.1:$HTTP --rtmp 127.0.0.1:$RTMP --srt 127.0.0.1:$SRT --capture-synthetic --rtsp 127.0.0.1:$RTSP --public-rtsp-url rtsp://127.0.0.1:$RTSP \
    --public-rtmp-url rtmp://127.0.0.1:$RTMP --public-http-url http://127.0.0.1:$HTTP --reconnect-grace "${GRACE:-3000}" \
    --codec-lib-dir "$CODEC_LIB_DIR" --codec-workers 2 --egress-allow 127.0.0.0/8 "$@" >>"$W/node.log" 2>&1 &
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
if [[ $ONLY == *H* ]]; then
  # H. 入力・出力: 外部への RTMP push（秘密値で鍵を渡す）、RTMP pull、HLS pull、宛先の制限（SSRF）
  P1=$(port)
  "$FFMPEG" -v error -listen 1 -i "rtmp://127.0.0.1:$P1/app/sinkkey" -c copy -f flv "$W/pushed.flv" & SINK=$!
  sleep 0.5
  new_stream h false
  api POST /secrets -d '{"name":"sinkkey","value":"sinkkey"}' >/dev/null
  api POST /streams/$(sid h)/outputs -d "{\"protocol\":\"rtmp_push\",\"url\":\"rtmp://127.0.0.1:$P1/app\",\"credential_ref\":\"secret:sinkkey\"}" > "$W/out.json"
  publish "$(key h)"; wait $SINK || true
  frames "$W/pushed.flv" v > "$W/pushed.v"
  # 出力は配信が始まってからつなぐので、先頭の GOP を逃すことがある。連続した一部として一致すること
  contiguous "$W/src.v" "$W/pushed.v" 300 && pass "H RTMP push to external server = contiguous source run ($(wc -l < "$W/pushed.v") frames)" || fail "H push differs ($(wc -l < "$W/pushed.v"))"
  # RTMP pull: ffmpeg が play 用の RTMP サーバーになる
  P2=$(port)
  "$FFMPEG" -v error -re -i "$W/src.flv" -c copy -f flv -listen 1 "rtmp://127.0.0.1:$P2/app/cam" & SRC=$!
  sleep 0.5
  new_stream hp false
  api POST /streams/$(sid hp)/inputs -d "{\"protocol\":\"rtmp_pull\",\"url\":\"rtmp://127.0.0.1:$P2/app/cam\"}" > "$W/in.json"
  wait $SRC || true
  # 再接続の猶予が切れて ENDLIST が付くまで待つ（pull は相手が消えると再接続を試み続ける）
  sleep $(( ${GRACE:-3000} / 1000 + 2 ))
  URL=$(api POST /streams/$(sid hp)/playback-url | json "['playback']['hls']")
  frames "$URL" v > "$W/pull.v"
  contiguous "$W/src.v" "$W/pull.v" 150 && pass "H RTMP pull = contiguous source run ($(wc -l < "$W/pull.v") frames)" || fail "H RTMP pull mismatch ($(wc -l < "$W/pull.v"))"
  # HLS pull: 外部の HLS（VOD）を取り込む
  mkdir -p "$W/hls"; "$FFMPEG" -v error -i "$W/src.flv" -c copy -f hls -hls_time 2 -hls_playlist_type vod "$W/hls/x.m3u8"
  P3=$(port); (cd "$W/hls" && exec python3 -m http.server $P3 --bind 127.0.0.1 >/dev/null 2>&1) & HTTPD=$!
  sleep 0.5
  new_stream hh false
  api POST /streams/$(sid hh)/inputs -d "{\"protocol\":\"hls_pull\",\"url\":\"http://127.0.0.1:$P3/x.m3u8\"}" >/dev/null
  # 実時間で送るので 12 秒＋猶予を待つ（始まる前も waiting_input なので、先に live を待つ）
  for _ in $(seq 40); do api GET /streams/$(sid hh) | grep -q '"observed_state":"live"' && break; sleep 0.25; done
  for _ in $(seq 60); do api GET /streams/$(sid hh) | grep -q '"observed_state":"waiting_input"' && break; sleep 0.5; done
  sleep 1
  kill $HTTPD 2>/dev/null || true
  URL=$(api POST /streams/$(sid hh)/playback-url | json "['playback']['hls']")
  frames "$URL" v > "$W/hpull.v"
  cmp -s "$W/src.v" "$W/hpull.v" && pass "H HLS pull bit-identical" || fail "H HLS pull differs ($(wc -l < "$W/hpull.v"))"
  # 宛先の制限: メタデータのアドレスは拒否
  new_stream hs false
  I=$(api POST /streams/$(sid hs)/inputs -d '{"protocol":"hls_pull","url":"http://169.254.169.254/latest/x.m3u8"}' | json "['id']")
  sleep 2
  api GET /inputs/$I | grep -q '許されていない宛先' && pass "H egress to metadata address refused" || fail "H SSRF not refused"
  c=$(api POST /streams/$(sid hs)/inputs -d '{"protocol":"rtmp_pull","url":"rtmp://u:p@h/app/k"}' -o /dev/null -w '%{http_code}')
  [ "$c" = 400 ] && pass "H credentials in URL rejected" || fail "H url creds $c"
  grep -q '"value"\|sinkkey' "$W/data/control/log" && grep -q '"sealed"' "$W/data/control/log" && ! grep -q 'secret:sinkkey.*sinkkey"' /dev/null; \
    python3 -c "import sys;d=open('$W/data/control/log','rb').read();sys.exit(1 if b'\"value\"' in d else 0)" && pass "H secret value not stored in plain" || fail "H secret stored in plain"
fi
if [[ $ONLY == *I* ]]; then
  # I. SRT（publish・play・pull・push、パスフレーズ）と RTSP pull（mediamtx）
  "$FFMPEG" -v error -i "$W/src.flv" -c copy -f mpegts "$W/src.ts"
  new_stream i1 false
  "$FFMPEG" -v error -re -i "$W/src.ts" -c copy -f mpegts "srt://127.0.0.1:$SRT?mode=caller&streamid=publish:$(key i1)&latency=200000" || true
  sleep $(( ${GRACE:-3000} / 1000 + 1 ))
  URL=$(api POST /streams/$(sid i1)/playback-url | json "['playback']['hls']")
  frames "$URL" v | head -n -3 > "$W/i1.v"
  # libsrt は閉じるときに送り残しを捨てるので、末尾が欠けうる。元素材の連続した一部であること
  contiguous "$W/src.v" "$W/i1.v" 300 && pass "I SRT publish (libsrt) = contiguous source run ($(wc -l < "$W/i1.v") frames)" || fail "I SRT publish differs ($(wc -l < "$W/i1.v"))"
  "$FFMPEG" -v error -re -i "$W/src.ts" -c copy -f mpegts "srt://127.0.0.1:$SRT?mode=caller&streamid=publish:sk_bogus" 2>/dev/null && fail "I SRT bad key accepted" || pass "I SRT bad key rejected"
  # テナントのパスフレーズ
  api POST /secrets -d '{"name":"srt-passphrase","value":"0123456789abcdef"}' >/dev/null
  new_stream i2 false
  "$FFMPEG" -v error -re -i "$W/src.ts" -t 3 -c copy -f mpegts "srt://127.0.0.1:$SRT?mode=caller&streamid=publish:$(key i2)" 2>/dev/null && fail "I SRT plaintext accepted when passphrase required" || pass "I SRT plaintext refused (passphrase required)"
  "$FFMPEG" -v error -re -i "$W/src.ts" -c copy -f mpegts "srt://127.0.0.1:$SRT?mode=caller&streamid=publish:$(key i2)&passphrase=0123456789abcdef&pbkeylen=32" || true
  sleep $(( ${GRACE:-3000} / 1000 + 1 ))
  URL=$(api POST /streams/$(sid i2)/playback-url | json "['playback']['hls']")
  frames "$URL" v | head -n -3 > "$W/i2.v"
  contiguous "$W/src.v" "$W/i2.v" 300 && pass "I SRT encrypted publish (AES-256) = contiguous source run ($(wc -l < "$W/i2.v") frames)" || fail "I SRT encrypted differs ($(wc -l < "$W/i2.v"))"
  # SRT play（署名付き）
  new_stream i3 false
  "$FFMPEG" -v error -re -i "$W/src.flv" -c copy -f flv rtmp://127.0.0.1:$RTMP/live/$(key i3) & PUB=$!; sleep 2
  PL=$(api POST /streams/$(sid i3)/playback-url | json "['playback']['rtmp']"); Q=$(python3 -c "import urllib.parse,sys;print(urllib.parse.quote(sys.argv[1],safe=''))" "${PL#*/play/}")
  timeout -k 5 20 "$FFMPEG" -v error -i "srt://127.0.0.1:$SRT?mode=caller&streamid=play:$Q&passphrase=0123456789abcdef" -c copy -f mpegts "$W/i3.ts" || true
  wait $PUB || true
  frames "$W/i3.ts" v > "$W/i3.v"
  contiguous "$W/src.v" "$W/i3.v" 150 && pass "I SRT play = contiguous source run ($(wc -l < "$W/i3.v") frames)" || fail "I SRT play mismatch ($(wc -l < "$W/i3.v"))"
  # SRT push（出力）と pull（入力）
  P4=$(port)
  timeout -k 5 40 "$FFMPEG" -v error -i "srt://127.0.0.1:$P4?mode=listener" -c copy -f mpegts "$W/i4.ts" & SINK=$!; sleep 0.5
  new_stream i4 false
  api POST /streams/$(sid i4)/outputs -d "{\"protocol\":\"srt_push\",\"url\":\"srt://127.0.0.1:$P4\"}" >/dev/null
  publish "$(key i4)"; sleep 2; kill $SINK 2>/dev/null; wait $SINK 2>/dev/null || true
  frames "$W/i4.ts" v | head -n -1 > "$W/i4.v"
  contiguous "$W/src.v" "$W/i4.v" 250 && pass "I SRT push to libsrt listener = contiguous source run ($(wc -l < "$W/i4.v") frames)" || fail "I SRT push mismatch ($(wc -l < "$W/i4.v"))"
  P5=$(port)
  "$FFMPEG" -v error -re -i "$W/src.ts" -c copy -f mpegts "srt://127.0.0.1:$P5?mode=listener" & SRC=$!; sleep 0.5
  new_stream i5 false
  api POST /streams/$(sid i5)/inputs -d "{\"protocol\":\"srt_pull\",\"url\":\"srt://127.0.0.1:$P5\"}" >/dev/null
  wait $SRC || true; sleep $(( ${GRACE:-3000} / 1000 + 2 ))
  URL=$(api POST /streams/$(sid i5)/playback-url | json "['playback']['hls']")
  frames "$URL" v | head -n -3 > "$W/i5.v"
  contiguous "$W/src.v" "$W/i5.v" 250 && pass "I SRT pull from libsrt listener = contiguous source run ($(wc -l < "$W/i5.v") frames)" || fail "I SRT pull mismatch ($(wc -l < "$W/i5.v"))"
  # RTSP pull（mediamtx）
  if docker image inspect bluenviron/mediamtx >/dev/null 2>&1; then
    RP=$(port)
    CID=$(docker run -d --rm --network host -e MTX_RTSPADDRESS=127.0.0.1:$RP -e MTX_RTMP=no -e MTX_HLS=no -e MTX_WEBRTC=no -e MTX_SRT=no bluenviron/mediamtx)
    sleep 1
    "$FFMPEG" -v error -re -stream_loop 1 -i "$W/src.flv" -c copy -f rtsp -rtsp_transport tcp "rtsp://127.0.0.1:$RP/cam" & SRC=$!
    sleep 1
    new_stream i6 false
    api POST /streams/$(sid i6)/inputs -d "{\"protocol\":\"rtsp_pull\",\"url\":\"rtsp://127.0.0.1:$RP/cam\"}" >/dev/null
    sleep 10
    URL=$(api POST /streams/$(sid i6)/playback-url | json "['playback']['hls']")
    api GET /streams/$(sid i6) | json "['live']" > "$W/i6.live"
    kill $SRC 2>/dev/null; wait $SRC 2>/dev/null || true; docker stop "$CID" >/dev/null 2>&1 || true
    sleep $(( ${GRACE:-3000} / 1000 + 2 ))
    frames "$URL" v > "$W/i6.v"
    contiguous "$W/src.v" "$W/i6.v" 100 || python3 - "$W/src.v" "$W/i6.v" <<'PY' && pass "I RTSP pull (mediamtx, TCP) frames from source ($(wc -l < "$W/i6.v"))" || fail "I RTSP pull mismatch ($(wc -l < "$W/i6.v"))"
import sys
s=set(open(sys.argv[1]).read().split()); p=open(sys.argv[2]).read().split()
sys.exit(0 if len(p)>=100 and sum(x in s for x in p) >= 0.98*len(p) else 1)
PY
  else
    echo "SKIP I RTSP (no mediamtx image)"
  fi
fi
if [[ $ONLY == *J* ]]; then
  # J. パイプライン（M5）: VP9 の 2 本を PinP に合成して AV1 で出す。オペレーターの操作。
  if [ ! -e "$CODEC_LIB_DIR/libvpx.so.12" ]; then echo "SKIP J (no libvpx)"; else
  for n in 1 2; do
    "$FFMPEG" -v error -f lavfi -i "testsrc2=size=640x360:rate=30" -f lavfi -i "sine=frequency=$((300*n)):sample_rate=48000" -t 12 \
      -c:v libvpx-vp9 -deadline realtime -cpu-used 8 -g 30 -b:v 800k -c:a libopus "$W/j$n.mp4"
    "$TARGET/debug/mp4-to-flv" "$W/j$n.mp4" "$W/j$n.flv"
  done
  api POST /streams -d '{"name":"jt","codec_profile":"royalty_free","hls":{"container":"fmp4"}}' > "$W/jt.json"
  api POST /streams -d '{"name":"jb","codec_profile":"royalty_free","hls":{"container":"fmp4"}}' > "$W/jb.json"
  api POST /streams -d '{"name":"jout","codec_profile":"royalty_free","hls":{"container":"fmp4"}}' > "$W/jout.json"
  cat > "$W/graph.json" <<J
{"version":1,"options":{"clock":"realtime"},"nodes":[
 {"type":"source","id":"t","stream":"$(sid jt)","video":{"codec":"vp9"},"audio":{"codec":"opus"}},
 {"type":"source","id":"b","stream":"$(sid jb)","video":{"codec":"vp9"}},
 {"type":"decode","id":"dt","input":"t.video"},{"type":"decode","id":"db","input":"b.video"},{"type":"decode","id":"at","input":"t.audio"},
 {"type":"compose","id":"prod","inputs":{"board":"db","teacher":"dt"},"canvas":{"width":640,"height":360,"fps_num":30},
  "scenes":{"pinp":{"layers":[{"source":"board","x":0,"y":0,"width":640,"height":360},{"source":"teacher","x":440,"y":250,"width":192,"height":108,"z":1}]},
            "teacher":{"layers":[{"source":"teacher","x":0,"y":0,"width":640,"height":360}]}},
  "program":"pinp","preview":"teacher","transition":{"kind":"fade","duration_ms":300}},
 {"type":"audio_mix","id":"mix","inputs":{"mic":{"from":"at","gain_db":0}}},
 {"type":"encode","id":"venc","input":"prod","codec":"av1","bitrate_kbps":800,"gop":30},
 {"type":"encode","id":"aenc","input":"mix","codec":"opus","bitrate_kbps":96},
 {"type":"sink","id":"out","inputs":["venc","aenc"],"stream":"$(sid jout)"}]}
J
  python3 -c "import json,sys;print(json.dumps({'name':'pinp','graph':json.load(open(sys.argv[1])),'desired_state':'running'}))" "$W/graph.json" > "$W/pbody.json"
  c=$(api POST /pipelines --data-binary @"$W/pbody.json" -o "$W/pipe.json" -w '%{http_code}')
  [ "$c" = 201 ] && pass "J pipeline created" || { fail "J create $c: $(cat "$W/pipe.json")"; }
  PID=$(json "['id']" < "$W/pipe.json")
  "$TARGET/debug/examples/rtmp_push" 127.0.0.1:$RTMP live "$(key jt)" "$W/j1.flv" >/dev/null 2>&1 &
  "$TARGET/debug/examples/rtmp_push" 127.0.0.1:$RTMP live "$(key jb)" "$W/j2.flv" >/dev/null 2>&1 &
  # rtmp_push は速く送るので、出力が出るまで待つ
  for _ in $(seq 60); do api GET /streams/$(sid jout) | grep -q '"observed_state":"live"' && break; sleep 0.5; done
  api GET /streams/$(sid jout) | grep -q '"observed_state":"live"' && pass "J output stream is live" || fail "J output not live: $(api GET /pipelines/$PID | head -c 400)"
  c=$(api POST /pipelines/$PID/control -d '{"op":"take","node":"prod"}' -o /dev/null -w '%{http_code}')
  [ "$c" = 200 ] && pass "J operator take (preview → program)" || fail "J control $c"
  sleep 4
  URL=$(api POST /streams/$(sid jout)/playback-url | json "['playback']['hls']")
  # 合成の出力は実時間で出続ける（入力が終わっても）。6 秒だけ読む
  n=$({ "$FFMPEG" -v error -t 6 -i "$URL" -map 0:v -f framemd5 - 2>/dev/null || true; } | grep -vc "^#" || true)
  [ "$n" -ge 30 ] && pass "J composed AV1 output decodes ($n frames)" || fail "J output frames $n"
  api GET /pipelines/$PID | json "['observed_state']" | grep -q running && pass "J pipeline running" || fail "J state"
  c=$(api POST /pipelines -d '{"name":"bad","graph":{"version":1,"nodes":[{"type":"source","id":"s","stream":"'"$(sid jt)"'","video":{"codec":"h264"}},{"type":"decode","id":"d","input":"s.video"},{"type":"encode","id":"e","input":"d","codec":"h264","bitrate_kbps":500},{"type":"sink","id":"o","inputs":["e"],"stream":"'"$(sid jout)"'"}]}}' -o "$W/bad.json" -w '%{http_code}')
  [ "$c" = 400 ] || [ "$c" = 503 ] && pass "J H.264 pipeline rejected with reasons ($c)" || fail "J bad pipeline $c"
  api DELETE /pipelines/$PID >/dev/null
  fi
fi
if [[ $ONLY == *K* ]]; then
  # K. PaaS 連携（模擬の AWS MediaLive）: 外部の受け口の作成・記録・取得・後片付け、秘密が漏れない
  MP=$(port)
  "$TARGET/debug/examples/paas_mock" $MP > "$W/mock.out" 2> "$W/mock.err" & MOCK=$!
  sleep 0.5
  api POST /secrets -d '{"name":"aws","value":"AKIAMOCK0000000001:mock/secret+0000000000000000000000000000"}' >/dev/null
  new_stream k false
  O=$(api POST /streams/$(sid k)/outputs -d "{\"protocol\":\"paas\",\"provider\":\"aws_medialive\",\"credential_ref\":\"secret:aws\",\"options\":{\"region\":\"ap-northeast-1\",\"endpoint\":\"http://127.0.0.1:$MP\",\"allow_cidr\":\"0.0.0.0/0\"}}")
  OID=$(echo "$O" | json "['id']")
  for _ in $(seq 40); do r=$(api GET /outputs/$OID | json "['provider_resource']"); [ "$r" != None ] && break; sleep 0.25; done
  [[ "$r" == input=*";sg="* ]] && pass "K external input + security group created and recorded ($r)" || fail "K provider_resource=$r"
  c=$(api DELETE /outputs/$OID -o /dev/null -w '%{http_code}'); sleep 0.5
  grep -q "DELETE /prod/inputs/" "$W/mock.err" && grep -q "DELETE /prod/inputSecurityGroups/" "$W/mock.err" && pass "K output delete cleans up external resources" || fail "K cleanup ($c)"
  grep -q "mock/secret" "$W/node.log" "$W/data/control/log" && fail "K AWS secret leaked" || pass "K AWS secret not in logs/state"
  kill $MOCK 2>/dev/null || true
fi
if [[ $ONLY == *L* ]]; then
  # L. 字幕と翻訳（模擬の音声認識・翻訳）: 復号せずに音声を送り、WebVTT の HLS 字幕を出す
  CP=$(port)
  python3 "$ROOT/tools/interop/caption_mock.py" $CP 2> "$W/cmock.err" & CM=$!
  sleep 0.5
  api POST /secrets -d '{"name":"anthropic","value":"mock-anthropic-key"}' >/dev/null
  api POST /streams -d "{\"name\":\"l\",\"captions\":{\"asr_provider\":\"openai\",\"asr_endpoint\":\"http://127.0.0.1:$CP/v1\",\"language\":\"ja\",\"translate_provider\":\"anthropic\",\"translate_endpoint\":\"http://127.0.0.1:$CP\",\"translate_credential_ref\":\"secret:anthropic\",\"targets\":[\"en\"]}}" > "$W/l.json"
  "$FFMPEG" -v error -re -f lavfi -i testsrc2=size=320x240:rate=30 -f lavfi -i sine=frequency=500:sample_rate=48000 -t 20 \
    -c:v libx264 -g 30 -c:a aac -f flv rtmp://127.0.0.1:$RTMP/live/$(key l) & PUB=$!
  sleep 16
  URL=$(api POST /streams/$(sid l)/playback-url | json "['playback']['hls']"); B=${URL%index.m3u8}
  curl -s "${B}master.m3u8" > "$W/master.m3u8"
  grep -q 'TYPE=SUBTITLES' "$W/master.m3u8" && grep -q 'LANGUAGE="ja"' "$W/master.m3u8" && grep -q 'LANGUAGE="en"' "$W/master.m3u8" && pass "L master playlist has ja/en subtitle renditions" || fail "L master: $(cat "$W/master.m3u8" | head -5)"
  curl -s "${B}sub_ja.m3u8" > "$W/sub_ja.m3u8"; V=$(grep -m1 '\.vtt' "$W/sub_ja.m3u8" || true)
  ok=0; for f in $(grep '\.vtt' "$W/sub_ja.m3u8"); do curl -s "${B}$f" | grep -q 'テスト' && ok=1; done
  [ $ok = 1 ] && pass "L Japanese captions in WebVTT segments" || fail "L no ja cues ($V)"
  curl -s "${B}sub_en.m3u8" > "$W/sub_en.m3u8"
  ok=0; for f in $(grep '\.vtt' "$W/sub_en.m3u8"); do curl -s "${B}$f" | grep -q 'EN:' && ok=1; done
  [ $ok = 1 ] && pass "L translated English captions" || fail "L no en cues"
  grep -q "POST /v1/audio/transcriptions" "$W/cmock.err" && pass "L audio chunks sent to ASR without decoding" || fail "L no ASR calls"
  grep -q "mock-anthropic-key" "$W/node.log" && fail "L API key leaked" || pass "L API key not logged"
  wait $PUB || true; kill $CM 2>/dev/null || true
fi
if [[ $ONLY == *M* ]]; then
  # M. RTSP サーバーの play（署名付き）: FFmpeg を RTSP のクライアントにして見る
  new_stream m false
  "$FFMPEG" -v error -re -i "$W/src.flv" -c copy -f flv rtmp://127.0.0.1:$RTMP/live/$(key m) & PUB=$!; sleep 2
  R=$(api POST /streams/$(sid m)/playback-url | json "['playback']['rtsp']")
  { timeout -k 5 15 "$FFMPEG" -v error -rtsp_transport tcp -i "$R" -t 5 -map 0:v -f framemd5 - 2>/dev/null || true; } | { grep -v "^#" || true; } | awk -F, '{gsub(/ /,"",$6); print $6}' > "$W/m.v"
  wait $PUB || true
  contiguous "$W/src.v" "$W/m.v" 90 && pass "M RTSP play (TCP) = contiguous source run ($(wc -l < "$W/m.v") frames)" || fail "M RTSP play mismatch ($(wc -l < "$W/m.v"))"
  timeout -k 2 5 "$FFMPEG" -v error -rtsp_transport tcp -i "rtsp://127.0.0.1:$RTSP/live/$(sid m)?exp=1&sig=AAAA" -t 1 -f null - 2>/dev/null && fail "M bad signature accepted" || pass "M bad signature rejected"
fi
if [[ $ONLY == *N* ]]; then
  # N. H.265（Enhanced RTMP で受けて、TS・fMP4 の HLS へそのまま通す。符号化・復号はしない）
  "$FFMPEG" -v error -f lavfi -i testsrc2=size=640x360:rate=30 -f lavfi -i sine=sample_rate=48000 -t 6 \
    -c:v libx265 -x265-params log-level=error -g 30 -bf 2 -c:a aac "$W/h265.mp4"
  "$TARGET/debug/mp4-to-flv" "$W/h265.mp4" "$W/h265.flv"
  frames "$W/h265.mp4" v > "$W/h265.v"
  for C in ts fmp4; do
    api POST /streams -d "{\"name\":\"n$C\",\"hls\":{\"container\":\"$C\"}}" > "$W/n$C.json"
    "$TARGET/debug/examples/rtmp_push" 127.0.0.1:$RTMP live "$(key n$C)" "$W/h265.flv" >/dev/null 2>&1
    sleep $(( ${GRACE:-3000} / 1000 + 1 ))
    URL=$(api POST /streams/$(sid n$C)/playback-url | json "['playback']['hls']")
    frames "$URL" v > "$W/n$C.v"
    cmp -s "$W/h265.v" "$W/n$C.v" && pass "N H.265 → HLS $C bit-identical" || fail "N H.265 $C ($(wc -l < "$W/n$C.v"))"
  done
fi
if [[ $ONLY == *O* ]]; then
  # O. キャプチャ機器（合成デバイス、別プロセスで取り込み・VP9 で符号化）→ stream → fMP4 の HLS
  if [ ! -e "$CODEC_LIB_DIR/libvpx.so.12" ]; then echo "SKIP O (no libvpx)"; else
  api GET /devices | grep -q '"synthetic' && pass "O capture devices listed in GET /devices" || fail "O devices: $(api GET /devices | head -c 300)"
  api POST /streams -d '{"name":"o","codec_profile":"royalty_free","hls":{"container":"fmp4"}}' > "$W/o.json"
  api POST /streams/$(sid o)/inputs -d '{"protocol":"device","url":"device:synthetic:0","options":{"codec":"vp9","width":"640","height":"360","fps":"30","bitrate_bps":"800000","audio_device":"synthetic:0"}}' > "$W/oi.json"
  sleep 8
  URL=$(api POST /streams/$(sid o)/playback-url | json "['playback']['hls']")
  n=$({ timeout -k 2 10 "$FFMPEG" -v error -i "$URL" -t 4 -map 0:v -f framemd5 - 2>/dev/null || true; } | grep -vc '^#' || true)
  [ "$n" -ge 60 ] && pass "O device → VP9 → live HLS decodes ($n frames)" || fail "O frames $n; input: $(api GET /inputs/$(json "['id']" < "$W/oi.json") | head -c 300)"
  api DELETE /inputs/$(json "['id']" < "$W/oi.json") >/dev/null
  fi
fi
summary
