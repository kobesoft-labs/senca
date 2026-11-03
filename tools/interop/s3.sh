#!/usr/bin/env bash
# 録画の保存先（S3 互換、ADR-0005）。S3 互換のサーバー（minio/minio、無ければ rustfs/rustfs）を Docker で動かす。
#   A. 録画 → 保存先へ上げる → 手元の写しを消す → VOD・Range・MP4 書き出し（分割アップロード）が保存先から元素材とビット一致
#   T. サムネイルも保存先から（AV1、dav1d があるとき）
#   R. 保持期限で保存先のオブジェクトも消える
#   C. 2 台のクラスタ: node 1 の録画を node 2 が保存先から出す（案内しない）。node 2 で消すと node 1 が消す
# 保存先のオブジェクトは curl の SigV4（--aws-sigv4、別の実装）で確かめる。
# 使い方: tools/interop/s3.sh（KEEP=1 で作業場所を残す、ONLY=ATRC で場面を絞る、S3_IMAGE で像を指定）
source "$(dirname "$0")/lib.sh"
[ -n "${NODE_BIN_PREBUILT:-}" ] || (cd "$ROOT" && cargo build -q -p senca-node -p senca-codec-worker -p senca-devtools && cargo build -q -p senca-rtmp --examples)
CODEC_LIB_DIR=${CODEC_LIB_DIR:-$HOME/tools/codeclibs/usr/lib/x86_64-linux-gnu}
ONLY=${ONLY:-ATRC}
command -v docker >/dev/null || { echo "SKIP s3 (docker がありません)"; exit 0; }
IMG=${S3_IMAGE:-}
if [ -z "$IMG" ]; then
  for c in minio/minio rustfs/rustfs; do
    if docker image inspect "$c" >/dev/null 2>&1 || timeout 180 docker pull -q "$c" >/dev/null 2>&1; then IMG=$c; break; fi
  done
fi
[ -n "$IMG" ] || { echo "SKIP s3 (S3 互換の像がありません: minio/minio・rustfs/rustfs)"; exit 0; }
W=$(mktemp -d); PIDS=(); CT=senca-s3-$$
cleanup() {
  for p in "${PIDS[@]:-}"; do [ -n "$p" ] && kill "$p" 2>/dev/null || true; done; wait 2>/dev/null || true
  docker rm -f "$CT" >/dev/null 2>&1 || true
  [ -n "${KEEP:-}" ] && echo "kept $W" || rm -rf "$W"
}
trap cleanup EXIT
AK=senca-test-key; SK=senca-test-secret-$RANDOM$RANDOM$RANDOM
S3P=$(port); S3=http://127.0.0.1:$S3P; BUCKET=senca-rec
case "$IMG" in
  *minio*) docker run -d --name "$CT" -p 127.0.0.1:$S3P:9000 -e MINIO_ROOT_USER=$AK -e MINIO_ROOT_PASSWORD=$SK "$IMG" server /data >/dev/null ;;
  *) docker run -d --name "$CT" -p 127.0.0.1:$S3P:9000 -e RUSTFS_ACCESS_KEY=$AK -e RUSTFS_SECRET_KEY=$SK "$IMG" >/dev/null ;;
esac
# 別の SigV4 の実装（curl）で保存先を見る
s3c() { curl -s --aws-sigv4 "aws:amz:us-east-1:s3" --user "$AK:$SK" "$@" || true; }
ok=0; for _ in $(seq 120); do c=$(s3c -o /dev/null -w '%{http_code}' -X PUT "$S3/$BUCKET"); [ "$c" = 200 ] && { ok=1; break; }; sleep 0.5; done
[ $ok = 1 ] && pass "S3-compatible server up ($IMG), bucket created" || { fail "S3 server ($IMG) not ready: $c"; docker logs "$CT" 2>&1 | tail -20; summary; exit 1; }
# 接頭辞の下のキー（1 行 1 件、大きさ付き）
keys() { s3c "$S3/$BUCKET?list-type=2&prefix=$1" | python3 -c '
import sys, xml.etree.ElementTree as E
r = E.fromstring(sys.stdin.read()); ns = {"s": "http://s3.amazonaws.com/doc/2006-03-01/"}
for c in r.findall("s:Contents", ns): print(c.find("s:Key", ns).text, c.find("s:Size", ns).text)'; }
printf '{"access_key_id":"%s","secret_access_key":"%s"}' "$AK" "$SK" > "$W/s3.json"; chmod 600 "$W/s3.json"

# 元素材: 書き出しが 5 MiB を超えるように高めのビットレート（分割アップロードを通す）
"$FFMPEG" -v error -f lavfi -i testsrc2=size=1280x720:rate=30 -f lavfi -i sine=frequency=440:sample_rate=48000 -t 10 \
  -c:v libx264 -g 30 -bf 2 -pix_fmt yuv420p -b:v 8M -maxrate 8M -bufsize 8M -c:a aac -b:a 128k -f flv "$W/src.flv"
frames "$W/src.flv" v > "$W/src.v"; frames "$W/src.flv" a > "$W/src.a"
GRACE=2000

# ---- 単一ノード ----
HTTP=$(port); RTMP=$(port); API=http://127.0.0.1:$HTTP/api/v1
start_node() {
  "$NODE_BIN" --data-dir "$W/data" --http 127.0.0.1:$HTTP --rtmp 127.0.0.1:$RTMP --public-http-url http://127.0.0.1:$HTTP \
    --reconnect-grace $GRACE --retention-sweep 1 --codec-lib-dir "$CODEC_LIB_DIR" --codec-workers 1 \
    --recording-storage s3://$BUCKET/single --s3-endpoint "$S3" --s3-credentials "$W/s3.json" \
    --recording-keep-local 0 --s3-part-size 5242880 "$@" >>"$W/node.log" 2>&1 &
  NODE_PID=$!; PIDS+=("$NODE_PID")
  for _ in $(seq 100); do curl -sf http://127.0.0.1:$HTTP/readyz >/dev/null && return; sleep 0.1; done
  echo "node did not start"; cat "$W/node.log"; exit 1
}
stop_node() { kill "$NODE_PID"; wait "$NODE_PID" 2>/dev/null || true; }
start_node
TOKEN=$(cat "$W/data/control/bootstrap-admin-token")
H=(-H "Authorization: Bearer $TOKEN")
api() { local m=$1 p=$2; shift 2; curl -s -X "$m" "${H[@]}" -H 'Content-Type: application/json' "$API$p" "$@"; }
latest_rec() { api GET "/recordings?stream=$1" | python3 -c "import json,sys;i=json.load(sys.stdin)['items'];print(sorted(i,key=lambda r:r['started_at'])[-1]['id'])"; }
wait_state() { for _ in $(seq 100); do s=$(api GET /recordings/$1 | json "['state']"); [ "$s" = "$2" ] && return 0; sleep 0.1; done; return 1; }
wait_stored() { for _ in $(seq 150); do s=$(api GET /recordings/$1 | json "['storage']" 2>/dev/null); [ "$s" != None ] && [ -n "$s" ] && return 0; sleep 0.2; done; return 1; }
publish() { "$FFMPEG" -v error -re -i "${2:-$W/src.flv}" -c copy -f flv rtmp://127.0.0.1:$RTMP/live/$1; }

if [[ $ONLY == *A* ]]; then
  S=$(api POST /streams -d '{"name":"a","recording":{"enabled":true}}'); SID=$(echo "$S" | json "['id']")
  publish "$(echo "$S" | json "['ingest']['stream_key']")"; sleep $((GRACE / 1000 + 1))
  R=$(latest_rec "$SID"); wait_state "$R" finalized || fail "A not finalized"
  wait_stored "$R" && pass "A recording stored ($(api GET /recordings/$R | json "['storage']['uri']"))" || { fail "A not stored"; tail -20 "$W/node.log"; }
  sleep 0.5
  [ ! -e "$W/data/recordings/$R" ] && [ -z "$(find "$W/data/segments" -type f -not -path '*/orphans/*' 2>/dev/null)" ] \
    && pass "A local copy removed (keep-local 0)" || fail "A local copy still present"
  keys single/recordings/$R/ > "$W/a.keys"; keys single/segments/$SID/ > "$W/a.segkeys"
  grep -q "single/recordings/$R/index.jsonl" "$W/a.keys" && pass "A index uploaded" || fail "A index missing: $(cat "$W/a.keys")"
  s3c "$S3/$BUCKET/single/recordings/$R/index.jsonl" > "$W/a.index"
  nseg=$(grep -c '"t":"segment"' "$W/a.index"); nkey=$(grep -c "\"key\":\"single/segments/$SID/" "$W/a.index")
  [ "$nseg" -gt 3 ] && [ "$nseg" = "$nkey" ] && [ "$(wc -l < "$W/a.segkeys")" = "$nseg" ] \
    && pass "A $nseg segments uploaded, index carries object keys" || fail "A segments=$nseg keys=$nkey objects=$(wc -l < "$W/a.segkeys")"
  # 保存先の中身が索引の SHA-256 と一致（curl で取り出して確かめる）
  python3 - "$W/a.index" "$S3" "$BUCKET" "$AK" "$SK" <<'PY' && pass "A every stored segment matches its index SHA-256" || fail "A stored segment hash mismatch"
import hashlib, json, subprocess, sys
idx, s3, b, ak, sk = sys.argv[1:]
for line in open(idx):
    e = json.loads(line)
    if e["t"] == "segment":
        d = subprocess.run(["curl", "-s", "--aws-sigv4", "aws:amz:us-east-1:s3", "--user", f"{ak}:{sk}", f"{s3}/{b}/{e['key']}"], capture_output=True).stdout
        if hashlib.sha256(d).hexdigest() != e["sha256"] or len(d) != e["bytes"]:
            sys.exit(1)
PY
  V=$(api POST /recordings/$R/playback-url | json "['playback']['hls']")
  frames "$V" v > "$W/vod.v"; frames "$V" a > "$W/vod.a"
  cmp -s "$W/src.v" "$W/vod.v" && cmp -s "$W/src.a" "$W/vod.a" && pass "A VOD from storage bit-identical ($(wc -l < "$W/vod.v") frames)" || fail "A VOD differs"
  SEG=$(curl -s "$V" | grep -m1 '\.ts$')
  curl -s -o "$W/seg.full" "${V%index.m3u8}$SEG"
  c=$(curl -s -o "$W/seg.part" -w '%{http_code}' -H 'Range: bytes=100-1123' "${V%index.m3u8}$SEG")
  [ "$c" = 206 ] && [ "$(wc -c < "$W/seg.part")" = 1024 ] && cmp -s "$W/seg.part" <(tail -c +101 "$W/seg.full" | head -c 1024) \
    && pass "A Range request served from storage (206)" || fail "A Range $c $(wc -c < "$W/seg.part")"
  curl -s "${H[@]}" -o "$W/exp.mp4" "$API/recordings/$R/export.mp4"
  frames "$W/exp.mp4" v > "$W/exp.v"; frames "$W/exp.mp4" a > "$W/exp.a"
  cmp -s "$W/src.v" "$W/exp.v" && cmp -s "$W/src.a" "$W/exp.a" && pass "A MP4 export from storage bit-identical" || fail "A export differs"
  # 書き出しは保存先に置かれる（5 MiB を超えるので分割アップロード。ETag が -N で終わる）
  for _ in $(seq 100); do s3c -I "$S3/$BUCKET/single/recordings/$R/export.mp4" | grep -qi '^etag' && break; sleep 0.2; done
  hd=$(s3c -I "$S3/$BUCKET/single/recordings/$R/export.mp4")
  size=$(echo "$hd" | tr -d '\r' | awk -F': ' 'tolower($1)=="content-length"{print $2}')
  echo "$hd" | tr -d '\r' | grep -qiE '^etag: "?[0-9a-f]+-[0-9]+"?$' && [ "$size" = "$(wc -c < "$W/exp.mp4")" ] \
    && pass "A export stored by multipart upload ($size bytes)" || fail "A export object: $(echo "$hd" | grep -i etag) size=$size"
  s3c -o "$W/exp.s3" "$S3/$BUCKET/single/recordings/$R/export.mp4"
  curl -s "${H[@]}" -o "$W/exp2.mp4" "$API/recordings/$R/export.mp4"
  cmp -s "$W/exp.mp4" "$W/exp.s3" && cmp -s "$W/exp.mp4" "$W/exp2.mp4" && pass "A stored export is identical and reused" || fail "A stored export differs"
  # 再起動しても（手元に何も無くても）出せる
  stop_node; start_node
  frames "$V" v > "$W/vod2.v"
  cmp -s "$W/src.v" "$W/vod2.v" && pass "A VOD after restart (index read from storage)" || fail "A VOD after restart"
  [ "$(api GET /recordings/$R | json "['segments']")" = "$nseg" ] && pass "A recording info from stored index" || fail "A info $(api GET /recordings/$R)"
  AR=$R; ASID=$SID
fi

if [[ $ONLY == *T* ]]; then
  if [ ! -e "$CODEC_LIB_DIR/libdav1d.so.7" ]; then echo "SKIP T (no $CODEC_LIB_DIR/libdav1d.so.7)"; else
  "$FFMPEG" -v error -f lavfi -i testsrc2=size=640x360:rate=30 -f lavfi -i sine=sample_rate=48000 -t 6 \
    -c:v libaom-av1 -cpu-used 8 -row-mt 1 -g 30 -b:v 500k -c:a libopus "$W/av1.mp4"
  "$TARGET/debug/mp4-to-flv" "$W/av1.mp4" "$W/av1.flv"
  S=$(api POST /streams -d '{"name":"t","codec_profile":"royalty_free","hls":{"container":"fmp4"},"recording":{"enabled":true}}'); SID=$(echo "$S" | json "['id']")
  "$TARGET/debug/examples/rtmp_push" 127.0.0.1:$RTMP live "$(echo "$S" | json "['ingest']['stream_key']")" "$W/av1.flv" >/dev/null 2>&1
  sleep $((GRACE / 1000 + 1))
  R=$(latest_rec "$SID"); wait_state "$R" finalized || fail "T not finalized"
  wait_stored "$R" || fail "T not stored"
  sleep 0.5
  [ ! -e "$W/data/recordings/$R" ] && keys single/recordings/$R/ | grep -q thumbnail.jpg && pass "T thumbnail uploaded, local copy removed" || fail "T thumbnail object"
  c=$(curl -s -o "$W/t.jpg" -w '%{http_code}' "${H[@]}" "$API/recordings/$R/thumbnail")
  [ "$c" = 200 ] && "$FFPROBE" -v error -show_entries stream=width,height -of csv=p=0 "$W/t.jpg" | grep -q '640,360' \
    && [ "$(api GET /recordings/$R | json "['thumbnail']")" = True ] && pass "T thumbnail served from storage (640x360)" || fail "T thumbnail $c"
  frames "$W/av1.mp4" v > "$W/t.sv"; curl -s "${H[@]}" -o "$W/t.mp4" "$API/recordings/$R/export.mp4"; frames "$W/t.mp4" v > "$W/t.ev"
  cmp -s "$W/t.sv" "$W/t.ev" && pass "T fMP4 (init + m4s) export from storage bit-identical" || fail "T export differs"
  fi
fi

if [[ $ONLY == *R* ]]; then
  if [ -z "${AR:-}" ]; then
    S=$(api POST /streams -d '{"name":"r","recording":{"enabled":true}}'); ASID=$(echo "$S" | json "['id']")
    publish "$(echo "$S" | json "['ingest']['stream_key']")"; sleep $((GRACE / 1000 + 1))
    AR=$(latest_rec "$ASID"); wait_state "$AR" finalized; wait_stored "$AR" || fail "R not stored"
  fi
  [ -n "$(keys single/segments/$ASID/)" ] || fail "R nothing stored before retention"
  G=$(api GET /recordings/$AR | json "['generation']")
  api PATCH /recordings/$AR -H "If-Match: \"$G\"" -d '{"retention_until":"2000-01-01T00:00:00Z"}' >/dev/null
  for _ in $(seq 100); do c=$(api GET /recordings/$AR -o /dev/null -w '%{http_code}'); [ "$c" = 404 ] && break; sleep 0.2; done
  [ "$c" = 404 ] && pass "R recording deleted by retention" || fail "R recording still $c"
  # 削除は後ろで進む（保存先 → 手元 → 制御状態）
  for _ in $(seq 100); do [ -z "$(keys single/recordings/$AR/)" ] && break; sleep 0.2; done
  [ -z "$(keys single/recordings/$AR/)" ] && [ -z "$(keys single/segments/$ASID/)" ] \
    && pass "R retention removed the stored objects (index, segments, export)" || fail "R objects left: $(keys single/recordings/$AR/; keys single/segments/$ASID/ | head -3)"
fi
stop_node

if [[ $ONLY == *C* ]]; then
  # ---- 2 台のクラスタ: 同じ保存先 ----
  echo "0123456789abcdef-cluster-secret" > "$W/secret"
  declare -A CH CR CM CL
  for i in 1 2; do CH[$i]=$(port); CR[$i]=$(port); CM[$i]=$(port); CL[$i]=$(port); done
  for i in 1 2; do
    cat > "$W/c$i.json" <<J
{"node_id": $i, "listen": "127.0.0.1:${CR[$i]}", "relay_listen": "127.0.0.1:${CL[$i]}", "secret_file": "$W/secret",
 "peers": [{"id":1,"addr":"127.0.0.1:${CR[1]}","relay":"127.0.0.1:${CL[1]}","api":"http://127.0.0.1:${CH[1]}"},
           {"id":2,"addr":"127.0.0.1:${CR[2]}","relay":"127.0.0.1:${CL[2]}","api":"http://127.0.0.1:${CH[2]}"}]}
J
    "$NODE_BIN" --data-dir "$W/d$i" --http 127.0.0.1:${CH[$i]} --rtmp 127.0.0.1:${CM[$i]} --public-http-url http://127.0.0.1:${CH[$i]} \
      --reconnect-grace $GRACE --retention-sweep 1 --cluster-config "$W/c$i.json" --node-name n$i \
      --recording-storage s3://$BUCKET/cluster --s3-endpoint "$S3" --s3-credentials "$W/s3.json" --s3-part-size 5242880 >>"$W/n$i.log" 2>&1 &
    PIDS+=($!)
  done
  for i in 1 2; do ok=0; for _ in $(seq 150); do curl -sf http://127.0.0.1:${CH[$i]}/readyz >/dev/null && { ok=1; break; }; sleep 0.2; done
    [ $ok = 1 ] || { fail "C node $i not ready"; tail -20 "$W/n$i.log"; }; done
  for _ in $(seq 50); do [ -s "$W/d1/control/bootstrap-admin-token" ] && break; sleep 0.2; done
  CT1=$(cat "$W/d1/control/bootstrap-admin-token")
  capi() { local n=$1 m=$2 p=$3; shift 3; curl -s -X "$m" -H "Authorization: Bearer $CT1" -H 'Content-Type: application/json' "http://127.0.0.1:${CH[$n]}/api/v1$p" "$@"; }
  S=$(capi 1 POST /streams -d '{"name":"c","recording":{"enabled":true}}'); SID=$(echo "$S" | json "['id']")
  "$FFMPEG" -v error -re -i "$W/src.flv" -c copy -f flv rtmp://127.0.0.1:${CM[1]}/live/$(echo "$S" | json "['ingest']['stream_key']")
  sleep $((GRACE / 1000 + 1))
  R=""; for _ in $(seq 50); do R=$(capi 2 GET "/recordings?stream=$SID" | python3 -c "import json,sys;i=json.load(sys.stdin)['items'];print(i[0]['id'] if i and i[0]['storage'] else '')"); [ -n "$R" ] && break; sleep 0.3; done
  [ -n "$R" ] && pass "C node 1's recording stored, visible on node 2" || { fail "C not stored"; tail -20 "$W/n1.log"; }
  [ -e "$W/d1/recordings/$R/index.jsonl" ] && [ ! -e "$W/d2/recordings/$R" ] && pass "C files are local to node 1 only (keep-local default)" || fail "C local layout"
  V2=$(capi 2 POST /recordings/$R/playback-url | json "['playback']['hls']")
  c=$(curl -s -o /dev/null -w '%{http_code}' "$V2")
  [ "$c" = 200 ] && [[ "$V2" == *":${CH[2]}/"* ]] && pass "C node 2 serves the VOD itself (no redirect to node 1)" || fail "C node 2 VOD playlist $c"
  frames "$V2" v > "$W/c.v"; frames "$V2" a > "$W/c.a"
  cmp -s "$W/src.v" "$W/c.v" && cmp -s "$W/src.a" "$W/c.a" && pass "C VOD via node 2 from storage bit-identical" || fail "C VOD via node 2 differs"
  capi 2 GET /recordings/$R/export.mp4 -o "$W/c.mp4"; frames "$W/c.mp4" v > "$W/c.ev"
  cmp -s "$W/src.v" "$W/c.ev" && pass "C MP4 export via node 2 bit-identical" || fail "C export via node 2 differs"
  # node 2 で消す → 持ち主の node 1 が保存先と手元を消す
  c=$(capi 2 DELETE /recordings/$R -o /dev/null -w '%{http_code}')
  for _ in $(seq 100); do [ -z "$(keys cluster/recordings/$R/)" ] && [ ! -e "$W/d1/recordings/$R" ] && break; sleep 0.2; done
  [ "$c" = 202 ] && [ -z "$(keys cluster/recordings/$R/)" ] && [ -z "$(keys cluster/segments/$SID/)" ] && [ ! -e "$W/d1/recordings/$R" ] \
    && pass "C delete via node 2: owner node 1 removed stored and local copies" || fail "C delete via node 2 ($c)"
fi
summary
