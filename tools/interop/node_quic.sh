#!/usr/bin/env bash
# senca-node の QUIC の受け口（--quic）: HTTP/3・WebTransport・MoQ（draft-22）を独立実装と確かめる。
#
#   A. HTTP/3 の署名付き HLS: aioquic の HTTP/3 client で再生リスト・セグメントを取り、TCP と同じバイトか、
#      LL-HLS のブロッキング・リロード、HEAD、署名の誤りは 403、TCP の応答の Alt-Svc
#   B. HTTP/3 の管理 API: Bearer の GET・POST・PATCH・DELETE、トークンなしは 401、SSE は 501
#   C. MoQ の購読: moq-dev/moq の `moq export fmp4`（native QUIC・WebTransport、署名は URL の query か
#      --connect-token）が catalog と CMAF の object を受け、ffprobe・ffmpeg で復号できるか。`moq fetch` の catalog
#   D. 認可: 署名なし・誤った署名の購読は断る（moq-dev と senca の client）
#   E. Chrome（ov-chrome:latest、playwright-core）: HTTP/3 の頁と、WebTransport の MoQ の購読・署名なしの拒否
#   F. MoQ の送出: publish 鍵を AUTHORIZATION TOKEN に入れた CMAF の送出で stream が live になり HLS で見える、
#      誤った鍵は断る（送り手は senca の moq_interop。独立実装ではない）
#
# 使い方: tools/interop/node_quic.sh
#   KEEP=1          作業場所を残す
#   NO_BROWSER=1    E を飛ばす
#   MOQ_BIN_DIR=dir moq-dev の `moq` の場所（既定は quic_moq.sh と同じ $HOME/.cache/senca-quic-work/moq-target/debug。
#                   無ければ tools/interop/quic_moq.sh を MOQ_BUILD=1 で一度動かすと作られる）
#   AIOQUIC_PY=python  aioquic の入った Python（無ければ uv で $HOME/.cache に作る。quic_moq.sh と同じ）
set -euo pipefail
source "$(dirname "$0")/lib.sh"
HERE="$ROOT/tools/interop/node_quic"
CACHE="$HOME/.cache"
CARGO=${CARGO:-$HOME/.cargo/bin/cargo}
[ -n "${NODE_BIN_PREBUILT:-}" ] || (cd "$ROOT" && "$CARGO" build -q -p senca-node && "$CARGO" build -q -p senca-moq --examples)
MOQ_INTEROP=$TARGET/debug/examples/moq_interop
mkdir -p "$CACHE"
W=$(mktemp -d "$CACHE/senca-node-quic.XXXXXX")
NODE_PID=; PUB_PID=; CONTAINER=
cleanup() {
  [ -n "$CONTAINER" ] && docker stop "$CONTAINER" >/dev/null 2>&1 || true
  [ -n "$PUB_PID" ] && kill "$PUB_PID" 2>/dev/null || true
  [ -n "$NODE_PID" ] && kill "$NODE_PID" 2>/dev/null || true
  wait 2>/dev/null || true
  rm -f "/tmp/senca-ports-$$"
  if [ -n "${KEEP:-}" ]; then echo "kept $W"; else rm -rf "$W"; fi
}
trap cleanup EXIT INT TERM

# 20000〜29999 の、TCP と UDP の両方で空いている番号（同じ試験の中で同じ番号を 2 度渡さない）
qport() { python3 - "/tmp/senca-ports-$$" <<'PY'
import os, random, socket, sys
used_file = sys.argv[1]
used = set(open(used_file).read().split()) if os.path.exists(used_file) else set()
for _ in range(500):
    p = random.randint(20000, 29999)
    if str(p) in used:
        continue
    try:
        for kind in (socket.SOCK_STREAM, socket.SOCK_DGRAM):
            s = socket.socket(socket.AF_INET, kind); s.bind(("127.0.0.1", p)); s.close()
        open(used_file, "a").write(f"{p}\n"); print(p); break
    except OSError:
        continue
PY
}
jget() { python3 -c "import json,sys; d=json.loads(sys.stdin.read().strip().splitlines()[-1]); print(eval('d'+sys.argv[1]))" "$1"; }

# aioquic（quic_moq.sh と同じ置き場所）
PY=${AIOQUIC_PY:-}
if [ -z "$PY" ]; then
  V=$CACHE/senca-quic-work/venv
  if ! { [ -x "$V/bin/python" ] && "$V/bin/python" -c 'import aioquic' 2>/dev/null; }; then
    UV=$(command -v uv || echo "$HOME/.local/bin/uv")
    "$UV" venv -q "$V" 2>/dev/null || true
    "$UV" pip install -q --python "$V/bin/python" aioquic >/dev/null 2>&1 || true
  fi
  PY=$V/bin/python
fi
"$PY" -c 'import aioquic' 2>/dev/null || { echo "aioquic が使えません（AIOQUIC_PY を指定するか uv を入れてください）"; exit 1; }
MOQ_BIN_DIR=${MOQ_BIN_DIR:-$CACHE/senca-quic-work/moq-target/debug}
M=$MOQ_BIN_DIR/moq
[ -x "$M" ] || { echo "moq-dev の moq がありません: $M（MOQ_BUILD=1 tools/interop/quic_moq.sh で作れます）"; exit 1; }

# 証明書: 試験用の CA と、その CA が署名した ECDSA P-256 の証明書（10 日。Chrome の serverCertificateHashes は 14 日以内）
openssl req -x509 -newkey ec -pkeyopt ec_paramgen_curve:prime256v1 -nodes -keyout "$W/ca.key" -out "$W/ca.pem" -days 30 \
  -subj /CN=senca-node-quic-test-ca -addext basicConstraints=critical,CA:TRUE -addext keyUsage=critical,keyCertSign 2>/dev/null
openssl req -newkey ec -pkeyopt ec_paramgen_curve:prime256v1 -nodes -keyout "$W/leaf.key" -out "$W/leaf.csr" -subj /CN=localhost 2>/dev/null
printf 'subjectAltName=DNS:localhost,IP:127.0.0.1\nbasicConstraints=CA:FALSE\nextendedKeyUsage=serverAuth\n' > "$W/ext.cnf"
openssl x509 -req -in "$W/leaf.csr" -CA "$W/ca.pem" -CAkey "$W/ca.key" -CAcreateserial -out "$W/leaf.pem" -days 10 -extfile "$W/ext.cnf" 2>/dev/null
CA=$W/ca.pem

HTTP=$(qport); RTMP=$(qport); Q=$(qport)
API=http://127.0.0.1:$HTTP/api/v1
"$NODE_BIN" --data-dir "$W/data" --http 127.0.0.1:$HTTP --rtmp 127.0.0.1:$RTMP --quic 127.0.0.1:$Q \
  --tls-cert "$W/leaf.pem" --tls-key "$W/leaf.key" --public-http-url http://127.0.0.1:$HTTP --public-rtmp-url rtmp://127.0.0.1:$RTMP \
  >"$W/node.log" 2>&1 &
NODE_PID=$!
for _ in $(seq 50); do curl -sf http://127.0.0.1:$HTTP/readyz >/dev/null && break; sleep 0.1; done
curl -sf http://127.0.0.1:$HTTP/readyz >/dev/null || { echo "node did not start"; cat "$W/node.log"; exit 1; }
TOKEN=$(cat "$W/data/control/bootstrap-admin-token")
api() { local m=$1 p=$2; shift 2; curl -s -X "$m" -H "Authorization: Bearer $TOKEN" -H 'Content-Type: application/json' "$API$p" "$@"; }
h3() { "$PY" -I "$HERE/h3_client.py" "$@" --ca "$CA"; }
H3=https://127.0.0.1:$Q

# 配信: LL-HLS（latency_profile=low）の stream に RTMP で送り続ける
api POST /streams -d '{"name":"quic","latency_profile":"low","recording":{"enabled":false}}' > "$W/s.json"
SID=$(json "['id']" < "$W/s.json"); KEY=$(json "['ingest']['stream_key']" < "$W/s.json")
"$FFMPEG" -v error -re -f lavfi -i testsrc2=size=640x360:rate=30 -f lavfi -i sine=frequency=440:sample_rate=48000 -t 120 \
  -c:v libx264 -preset ultrafast -g 30 -pix_fmt yuv420p -c:a aac -b:a 128k -f flv "rtmp://127.0.0.1:$RTMP/live/$KEY" >"$W/ffmpeg.log" 2>&1 &
PUB_PID=$!
for _ in $(seq 100); do [ "$(api GET /streams/$SID | json "['observed_state']")" = live ] && break; sleep 0.1; done
sleep 5  # 再生リストにセグメントが溜まるまで
api POST /streams/$SID/playback-url > "$W/pb.json"
HLS=$(json "['playback']['hls']" < "$W/pb.json"); MOQ_URL=$(json "['playback']['moq']" < "$W/pb.json")
QS=${MOQ_URL#*\?}
HLS_PATH=${HLS#http://127.0.0.1:$HTTP}
[ "${MOQ_URL%%\?*}" = "https://127.0.0.1:$Q/moq" ] && pass "playback-url carries the MoQ WebTransport URL ($MOQ_URL)" || fail "playback-url moq: $MOQ_URL"

echo "== A. HTTP/3: signed HLS (aioquic)"
same=0
for _ in 1 2 3 4 5; do
  h3 "$H3$HLS_PATH" --out "$W/pl.h3" > "$W/pl.h3.json" || true
  curl -s -D "$W/pl.tcp.h" -o "$W/pl.tcp" "$HLS"
  if cmp -s "$W/pl.h3" "$W/pl.tcp" && [ -s "$W/pl.h3" ]; then same=1; break; fi
  sleep 0.3
done
[ $same = 1 ] && [ "$(jget "['alpn']" < "$W/pl.h3.json")" = h3 ] && pass "A playlist over HTTP/3 (ALPN h3) is byte-identical to TCP ($(wc -c < "$W/pl.h3") bytes)" || fail "A playlist h3 vs tcp: $(cat "$W/pl.h3.json")"
grep -qi "^alt-svc: h3=\":$Q\"" "$W/pl.tcp.h" && pass "A TCP response advertises Alt-Svc: h3=\":$Q\"" || fail "A no Alt-Svc on TCP: $(grep -i alt-svc "$W/pl.tcp.h" || true)"
SEG=$(grep -v '^#' "$W/pl.h3" | grep -v '^$' | head -1)
BASE=${HLS_PATH%index.m3u8}
h3 "$H3$BASE$SEG" --out "$W/seg.h3" > "$W/seg.h3.json" || true
curl -s -o "$W/seg.tcp" "${HLS%index.m3u8}$SEG"
if [ -s "$W/seg.h3" ] && cmp -s "$W/seg.h3" "$W/seg.tcp"; then pass "A segment $SEG over HTTP/3 is byte-identical to TCP ($(wc -c < "$W/seg.h3") bytes)"; else fail "A segment $SEG h3 vs tcp: $(cat "$W/seg.h3.json")"; fi
# ffprobe が終了コードで失敗を返しても、数えたフレームの数を使う（pipefail で 0 が足されて「60 と 0」の 2 行にならないように）
vframes() { local n; n=$({ "$FFPROBE" -v error -count_frames -select_streams v:0 -show_entries stream=nb_read_frames -of csv=p=0 "$1" 2>/dev/null || true; } | grep -m1 -E '^[0-9]+$' || true); echo "${n:-0}"; }
n=$(vframes "$W/seg.h3")
[ "${n:-0}" -gt 0 ] && pass "A segment fetched over HTTP/3 decodes ($n video frames)" || fail "A segment decode: $n"
# LL-HLS のブロッキング・リロード: 今の再生リストの次のセグメントの、さらに次のセグメントの part 0 を待つ
h3 "$H3$HLS_PATH" --out "$W/pl2.h3" > /dev/null || true
NEXT=$(python3 -c '
import re,sys
t=open(sys.argv[1]).read()
seq=int(re.search(r"#EXT-X-MEDIA-SEQUENCE:(\d+)",t).group(1)); print(seq+t.count("#EXTINF")+1)' "$W/pl2.h3")
h3 "$H3$HLS_PATH?_HLS_msn=$NEXT&_HLS_part=0" --out "$W/block.h3" --timeout 12 > "$W/block.json" || true
secs=$(jget "['seconds']" < "$W/block.json")
[ "$(jget "['status']" < "$W/block.json")" = 200 ] && grep -q "URI=\"$NEXT\.0\." "$W/block.h3" && python3 -c "import sys; sys.exit(0 if float(sys.argv[1]) > 0.2 else 1)" "$secs" \
  && pass "A LL-HLS blocking reload over HTTP/3 (_HLS_msn=$NEXT&_HLS_part=0) held ${secs}s and returned with the part" || fail "A blocking reload: $(cat "$W/block.json")"
o=$(h3 "$H3/healthz" --method HEAD --out "$W/head.body" || true)
[ "$(echo "$o" | jget "['status']")" = 200 ] && [ "$(echo "$o" | jget "['headers'].get('content-length')")" = 3 ] && [ ! -s "$W/head.body" ] \
  && pass "A HEAD /healthz over HTTP/3: 200, content-length 3, no body" || fail "A HEAD: $o"
BAD=$(echo "$HLS_PATH" | sed -E 's#^/s/([0-9]+)/[^/]+/#/s/\1/AAAAAAAAAAAAAAAAAAAAAA/#')
o=$(h3 "$H3$BAD" || true)
[ "$(echo "$o" | jget "['status']")" = 403 ] && pass "A wrong signature over HTTP/3 is 403" || fail "A bad signature: $o"

echo "== B. HTTP/3: management API (aioquic)"
o=$(h3 "$H3/api/v1/streams" -H "authorization: Bearer $TOKEN" --out "$W/list.h3" || true)
[ "$(echo "$o" | jget "['status']")" = 200 ] && grep -q "$SID" "$W/list.h3" && pass "B GET /api/v1/streams over HTTP/3 with Bearer: 200, lists $SID" || fail "B list: $o"
o=$(h3 "$H3/api/v1/streams" || true)
[ "$(echo "$o" | jget "['status']")" = 401 ] && pass "B no token over HTTP/3: 401" || fail "B no token: $o"
o=$(h3 "$H3/api/v1/streams" --method POST -H "authorization: Bearer $TOKEN" -H 'content-type: application/json' --data '{"name":"via-h3"}' --out "$W/new.h3" || true)
NID=$(json "['id']" < "$W/new.h3" 2>/dev/null || echo)
[ "$(echo "$o" | jget "['status']")" = 201 ] && [ -n "$NID" ] && pass "B POST /api/v1/streams over HTTP/3: 201 ($NID)" || fail "B POST: $o $(head -c 200 "$W/new.h3")"
o=$(h3 "$H3/api/v1/streams/$NID" --method PATCH -H "authorization: Bearer $TOKEN" -H 'content-type: application/json' -H 'if-match: "1"' --data '{"name":"renamed-h3"}' --out "$W/patch.h3" || true)
[ "$(echo "$o" | jget "['status']")" = 200 ] && [ "$(api GET /streams/$NID | json "['name']")" = renamed-h3 ] && pass "B PATCH over HTTP/3 with If-Match: 200, visible over TCP" || fail "B PATCH: $o"
o=$(h3 "$H3/api/v1/streams/$NID" --method DELETE -H "authorization: Bearer $TOKEN" || true)
c=$(curl -s -o /dev/null -w '%{http_code}' -H "Authorization: Bearer $TOKEN" "$API/streams/$NID")
[ "$(echo "$o" | jget "['status']")" = 204 ] && [ "$c" = 404 ] && pass "B DELETE over HTTP/3: 204, then 404 over TCP" || fail "B DELETE: $o (tcp $c)"
o=$(h3 "$H3/api/v1/events" -H "authorization: Bearer $TOKEN" || true)
[ "$(echo "$o" | jget "['status']")" = 501 ] && pass "B SSE over HTTP/3 is 501 (served over HTTP/1.1)" || fail "B SSE: $o"

echo "== C. MoQ subscribe (moq-dev $("$M" --version 2>/dev/null | head -1))"
# ノードは MoQ を WebSocket では受けない。moq-dev の WebSocket の予備の接続は切っておく（並んで失敗して結果が揺れるため）
MCON=(--connect-version moq-transport-22 --connect-tls-root "$CA" --connect-websocket-enabled=false)
export_fmp4() { # $1 = 名前, 残りは moq の引数
  local name=$1; shift
  timeout 8 "$M" "$@" "${MCON[@]}" --broadcast "senca/live/$SID" export fmp4 > "$W/$name.mp4" 2> "$W/$name.log" || true
}
check_cmaf() { # $1 = 名前, $2 = 説明
  local f=$W/$1.mp4 v a err
  v=$("$FFPROBE" -v error -count_frames -select_streams v:0 -show_entries stream=codec_name,nb_read_frames -of csv=p=0 "$f" 2>/dev/null || true)
  a=$("$FFPROBE" -v error -count_frames -select_streams a:0 -show_entries stream=codec_name,nb_read_frames -of csv=p=0 "$f" 2>/dev/null || true)
  err=$("$FFMPEG" -v error -i "$f" -f null - 2>&1 | head -3 || true)
  if [[ ${v:-} == h264,* ]] && [ "${v#h264,}" -gt 30 ] && [[ ${a:-} == aac,* ]] && [ "${a#aac,}" -gt 30 ] && [ -z "$err" ]; then
    pass "C $2: catalog + CMAF objects, decodes cleanly (video $v, audio $a, $(wc -c < "$f") bytes)"
  else fail "C $2: video='$v' audio='$a' ffmpeg='$err' $(sed 's/\x1b\[[0-9;]*m//g' "$W/$1.log" | grep -i 'error\|warn' | grep -v moq_sock | tail -2)"; fi
}
export_fmp4 native --connect "moqt://localhost:$Q/?$QS"
check_cmaf native "moq export fmp4 over native QUIC (signature in the URL query)"
export_fmp4 wt --connect "https://localhost:$Q/moq?$QS"
grep -q "connected.*webtransport" "$W/wt.log" && pass "C moq-dev connected over WebTransport (h3, moqt-22)" || fail "C moq-dev WebTransport connect: $(tail -2 "$W/wt.log")"
check_cmaf wt "moq export fmp4 over WebTransport"
export_fmp4 token --connect "moqt://localhost:$Q/" --connect-token "$QS"
check_cmaf token "moq export fmp4 with the signature as SETUP AUTHORIZATION TOKEN"
c=$(timeout 8 "$M" --connect "moqt://localhost:$Q/?$QS" "${MCON[@]}" --broadcast "senca/live/$SID" fetch catalog.json 2>"$W/fetch.log" || true)
echo "$c" | grep -q '"kind":"cmaf"' && echo "$c" | grep -q '"description"' && pass "C moq fetch catalog.json: hang catalog with CMAF init and codec description" || fail "C moq fetch catalog: $(echo "$c" | head -c 200) $(tail -1 "$W/fetch.log")"

echo "== D. MoQ authorization"
export_fmp4 noauth --connect "moqt://localhost:$Q/"
[ ! -s "$W/noauth.mp4" ] && grep -q "unauthorized" "$W/noauth.log" && pass "D moq-dev without signature: refused (unauthorized), no media" || fail "D unsigned moq-dev got $(wc -c < "$W/noauth.mp4") bytes"
BADQS=$(echo "$QS" | sed -E 's/sig=[^&]*/sig=AAAAAAAAAAAAAAAAAAAAAA/')
export_fmp4 badsig --connect "https://localhost:$Q/moq?$BADQS"
[ ! -s "$W/badsig.mp4" ] && grep -q "unauthorized" "$W/badsig.log" && pass "D moq-dev (WebTransport) with a wrong signature: refused, no media" || fail "D bad signature moq-dev got $(wc -c < "$W/badsig.mp4") bytes"
EXPQS=$(echo "$QS" | sed -E 's/exp=[0-9]*/exp=1/')
o=$(timeout 10 "$MOQ_INTEROP" subscribe --connect 127.0.0.1:$Q --ca "$CA" --name localhost --namespace "senca/live/$SID" --track video --seconds 4 --token "$EXPQS" || true)
echo "$o" | grep -q '"error": "0x1 unauthorized"' && pass "D SUBSCRIBE with an expired signature: REQUEST_ERROR UNAUTHORIZED (senca client)" || fail "D expired: $o"
o=$(timeout 10 "$MOQ_INTEROP" subscribe --connect 127.0.0.1:$Q --ca "$CA" --name localhost --namespace "senca/live/$SID" --track video --seconds 4 || true)
echo "$o" | grep -q '"error": "0x1 unauthorized"' && pass "D SUBSCRIBE without a token: REQUEST_ERROR UNAUTHORIZED (senca client)" || fail "D no token: $o"
o=$(timeout 10 "$MOQ_INTEROP" subscribe --connect 127.0.0.1:$Q --ca "$CA" --name localhost --namespace "senca/live/$SID" --track video --count 30 --seconds 6 --token "$QS" || true)
[ "$(echo "$o" | jget "['objects']")" -ge 30 ] && pass "D the same SUBSCRIBE with the signature as token: $o" || fail "D signed senca client: $o"

echo "== E. Chrome: HTTP/3 + MoQ over WebTransport"
IMG=
if [ -z "${NO_BROWSER:-}" ] && command -v docker >/dev/null && docker info >/dev/null 2>&1; then
  IMG=${BROWSER_IMAGE:-ov-chrome:latest}
  docker image inspect "$IMG" >/dev/null 2>&1 || IMG=
fi
if [ -n "$IMG" ]; then
  SPKI=$(openssl x509 -in "$W/leaf.pem" -pubkey -noout | openssl pkey -pubin -outform der | openssl dgst -sha256 -binary | base64)
  HASH=$(openssl x509 -in "$W/leaf.pem" -outform der | openssl dgst -sha256 -hex | awk '{print $2}')
  cp "$HERE/browser_moq_node.cjs" "$W/"
  CONTAINER=senca-node-quic-$$
  timeout 120 docker run --rm --name "$CONTAINER" --network host --ipc=host -v "$W:$W" "$IMG" bash -c \
    "node '$W/browser_moq_node.cjs' $Q '$SPKI' $HASH '$W/chrome.json' '$QS' senca/live/$SID video 30; chown -R $(id -u):$(id -g) '$W'" > "$W/browser.log" 2>&1 || true
  CONTAINER=
  if [ -s "$W/chrome.json" ]; then
    while IFS=$'\t' read -r ok name detail; do
      if [ "$ok" = True ]; then pass "E $name"; else fail "E $name — $detail"; fi
    done < <(python3 -c 'import json,sys
for c in json.load(open(sys.argv[1]))["checks"]: print("%s\t%s\t%s" % (c["ok"], c["name"], c["detail"].replace("\n"," ").replace("\t"," ")[:300]))' "$W/chrome.json")
  else fail "E browser produced no result: $(tail -3 "$W/browser.log")"; fi
else
  echo "SKIP E (no Docker / $IMG)"
fi

echo "== F. MoQ publish into a stream (stream key as AUTHORIZATION TOKEN)"
"$FFMPEG" -v error -y -f lavfi -i testsrc=size=640x360:rate=30 -t 6 -c:v libx264 -preset ultrafast -g 30 -keyint_min 30 -pix_fmt yuv420p \
  -movflags +frag_keyframe+empty_moov+default_base_moof -f mp4 "$W/moqpub.mp4"
api POST /streams -d '{"name":"moq-in","recording":{"enabled":false}}' > "$W/s2.json"
S2=$(json "['id']" < "$W/s2.json"); K2=$(json "['ingest']['stream_key']" < "$W/s2.json")
o=$(timeout 10 "$MOQ_INTEROP" publish --connect 127.0.0.1:$Q --ca "$CA" --name localhost --namespace "senca/live/$S2" --fmp4 "$W/moqpub.mp4" --seconds 3 --token "sk_wrongwrongwrongwrongwrongwrongwrongwrong" 2>&1 || true)
echo "$o" | grep -q "request error 0x1" && pass "F PUBLISH_NAMESPACE with a wrong stream key: UNAUTHORIZED" || fail "F wrong key: $o"
timeout 20 "$MOQ_INTEROP" publish --connect 127.0.0.1:$Q --ca "$CA" --name localhost --namespace "senca/live/$S2" --fmp4 "$W/moqpub.mp4" --seconds 8 --token "$K2" > "$W/mpub.out" 2> "$W/mpub.log" &
MP=$!
st=; for _ in $(seq 60); do st=$(api GET /streams/$S2 | python3 -c "import json,sys;d=json.load(sys.stdin);print(d['observed_state'], (d.get('live') or {}).get('transport'))"); [ "$st" = "live moq" ] && break; sleep 0.1; done
[ "$st" = "live moq" ] && pass "F stream is live with transport moq" || fail "F state: $st $(tail -2 "$W/mpub.log")"
sleep 3
U2=$(api POST /streams/$S2/playback-url | json "['playback']['hls']")
curl -s "$U2" > "$W/in.m3u8"
SEG2=$(grep -v '^#' "$W/in.m3u8" | grep -v '^$' | head -1 || true)
n=0; [ -n "$SEG2" ] && { curl -s -o "$W/in.seg" "${U2%index.m3u8}$SEG2"; n=$(vframes "$W/in.seg"); }
[ "${n:-0}" -gt 0 ] && pass "F CMAF published over MoQ plays back as HLS ($SEG2: $n frames)" || fail "F HLS from MoQ publish: $(head -c 300 "$W/in.m3u8")"
wait $MP 2>/dev/null || true

grep -q "panicked" "$W/node.log" && fail "node panicked: $(grep -m1 panicked "$W/node.log")" || pass "node log has no panic"
summary
