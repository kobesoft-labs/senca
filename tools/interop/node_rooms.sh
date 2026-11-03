#!/usr/bin/env bash
# ノードに組み込んだリアルタイムの部屋（ADR-0015）を Chrome で: REST で部屋を作り、ノードが発行した参加トークンで
# 3 つのページが `GET /signaling` から入る。互いの映像の復号、DataChannel の往復、サイマルキャストの層の選択、
# REST の参加者・統計・送信、SSE の部屋の出来事を確かめる。
#
# 使い方: tools/interop/node_rooms.sh（KEEP=1 で作業場所を残す）
set -euo pipefail
source "$(dirname "$0")/lib.sh"
IMG=${BROWSER_IMAGE:-ov-chrome:latest}
if ! docker image inspect "$IMG" >/dev/null 2>&1; then echo "SKIP: Docker image $IMG がない"; exit 0; fi
[ -n "${NODE_BIN_PREBUILT:-}" ] || (cd "$ROOT" && cargo build -q -p senca-node)
mkdir -p "$HOME/.cache"
W=$(mktemp -d -p "$HOME/.cache" senca-node-rooms.XXXXXX)
PIDS=(); C="senca-node-rooms-$$"
cleanup() {
  for p in "${PIDS[@]}"; do kill "$p" 2>/dev/null || true; done
  docker stop -t 2 "$C" >/dev/null 2>&1 || true
  wait 2>/dev/null || true
  if [ -n "${KEEP:-}" ]; then echo "kept $W"; else rm -rf "$W"; fi
}
trap cleanup EXIT
IP=${HOST_IP:-$(hostname -I | awk '{print $1}')}
HP=$(port); UP=$(port); PP=$(port)
"$NODE_BIN" --data-dir "$W/data" --http 127.0.0.1:$HP --rtmp off --webrtc-udp "$IP:$UP" --public-http-url "http://127.0.0.1:$HP" >"$W/node.log" 2>&1 &
PIDS+=("$!")
for _ in $(seq 100); do curl -sf "http://127.0.0.1:$HP/readyz" >/dev/null && break; sleep 0.1; done
TOKEN=$(cat "$W/data/control/bootstrap-admin-token")
api() { local m=$1 p=$2; shift 2; curl -s -X "$m" -H "Authorization: Bearer $TOKEN" -H 'Content-Type: application/json' "http://127.0.0.1:$HP/api/v1$p" "$@"; }
code() { local m=$1 p=$2; shift 2; curl -s -o /dev/null -w '%{http_code}' -X "$m" -H "Authorization: Bearer $TOKEN" -H 'Content-Type: application/json' "http://127.0.0.1:$HP/api/v1$p" "$@"; }

# SSE（部屋の出来事。部屋を作る前から聞く）
curl -sN -H "Authorization: Bearer $TOKEN" "http://127.0.0.1:$HP/api/v1/events" > "$W/sse.txt" & PIDS+=("$!")
sleep 0.3
# 部屋を作る（REST）
c=$(code POST /rooms -d '{"id":"demo","settings":{"max_participants":10,"video_codec":"vp8"},"metadata":{"title":"interop"}}')
[ "$c" = 201 ] && pass "room created via REST" || fail "create room $c"
c=$(code POST /rooms -d '{"id":"demo"}'); [ "$c" = 409 ] && pass "duplicate room id 409" || fail "dup room $c"
c=$(code POST /rooms -d '{"settings":{"max_participants":0}}'); [ "$c" = 400 ] && pass "invalid settings 400" || fail "bad settings $c"
for _ in $(seq 40); do st=$(api GET /rooms/demo | json "['placement_status']['state']"); [ "$st" = running ] && break; sleep 0.1; done
[ "$st" = running ] && pass "room placed on the grid ($st)" || fail "room placement $st"
grep -q "rooms: room demo hosted here" "$W/node.log" && pass "room hosted by this node" || fail "room not hosted"
# 参加トークン（client_id はトークンに入れる。名乗った値は信じない）
for n in alice bob carol; do
  api POST /rooms/demo/tokens -d "{\"role\":\"sendrecv\",\"client_id\":\"$n\",\"ttl_secs\":600}" > "$W/tok-$n.json"
done
T_ALICE=$(json "['token']" < "$W/tok-alice.json"); T_BOB=$(json "['token']" < "$W/tok-bob.json"); T_CAROL=$(json "['token']" < "$W/tok-carol.json")
SIG=$(json "['signaling_url']" < "$W/tok-alice.json")
[ "$SIG" = "ws://127.0.0.1:$HP/signaling" ] && pass "token response carries the signaling URL" || fail "signaling url $SIG"
# 無い部屋のトークンは部屋を自動で作る
c=$(code POST /rooms/lobby-auto/tokens -d '{"role":"recvonly"}'); [ "$c" = 201 ] && [ "$(api GET /rooms/lobby-auto | json "['auto']")" = True ] && pass "token for a new room auto-creates it" || fail "auto room $c"

# 試験ページ: 例のサーバーのページを、トークンと WebSocket の URL を query で受ける形にして配る
python3 - "$ROOT/crates/senca-rooms/examples/rooms_server.rs" "$W/www/index.html" <<'PY'
import re, sys, os
src = open(sys.argv[1]).read()
page = re.search(r'const PAGE: &str = r#"(.*?)"#;', src, re.S).group(1)
page = page.replace("const tok = await (await fetch(`/token?room=${room}&name=${name}`)).json();", "const tok = { token: q.get('token') };")
page = page.replace("ws = new WebSocket(`ws://${location.host}/signaling`, 'senca.rooms.v1');", "ws = new WebSocket(q.get('ws'), 'senca.rooms.v1');")
page = page.replace("client_id: name, ", "")
assert "q.get('token')" in page and "q.get('ws')" in page
os.makedirs(os.path.dirname(sys.argv[2]), exist_ok=True)
open(sys.argv[2], "w").write(page)
PY
(cd "$W/www" && exec python3 -m http.server $PP --bind 127.0.0.1 >/dev/null 2>&1) & PIDS+=("$!")
cp "$ROOT/tools/interop/rooms.sh" "$W/rooms.sh.ref"
# 操る道具は rooms.sh と同じもの（取り出す）
python3 - "$ROOT/tools/interop/rooms.sh" "$W/drive.cjs" <<'PY'
import re, sys
s = open(sys.argv[1]).read()
open(sys.argv[2], "w").write(re.search(r"cat > \"\$W/drive.cjs\" <<'EOF'\n(.*?)\nEOF\n", s, re.S).group(1))
PY
enc() { python3 -c "import urllib.parse,sys; print(urllib.parse.quote(sys.argv[1], safe=''))" "$1"; }
WS=$(enc "$SIG")
Q1="room=demo&name=alice&simulcast=1&peers=2&secs=60&token=$(enc "$T_ALICE")&ws=$WS"
Q2="room=demo&name=bob&peers=2&secs=60&token=$(enc "$T_BOB")&ws=$WS"
Q3="room=demo&name=carol&peers=2&secs=60&switch=1&target=alice&token=$(enc "$T_CAROL")&ws=$WS"
# 参加者がそろったところで REST を確かめる（ページが開いている間）
(
  for _ in $(seq 120); do n=$(api GET /rooms/demo/participants | json "['items'].__len__()" 2>/dev/null || echo 0); [ "$n" = 3 ] && break; sleep 0.5; done
  echo "$n" > "$W/rest_participants"
  sleep 8
  api GET /rooms/demo/stats > "$W/rest_stats.json"
  api POST /rooms/demo/messages -d '{"data":{"notice":"hello from REST"}}' > "$W/rest_push.json"
) & RESTP=$!
timeout 160 docker run --rm --name "$C" --network host -v "$W:$W" "$IMG" bash -c \
  "node '$W/drive.cjs' 'http://127.0.0.1:$PP' '$W/result.json' 90000 '$Q1' '$Q2' '$Q3'; chown $(id -u):$(id -g) '$W/result.json'" >"$W/browser.log" 2>&1 || true
wait $RESTP || true
R="$W/result.json"
jq_() { python3 -I -c "import json,sys;d=json.load(open(sys.argv[1]));print(eval(sys.argv[2]))" "$1" "$2"; }
if [ ! -s "$R" ]; then fail "no browser result ($(tail -5 "$W/browser.log"))"; summary; exit 1; fi
echo "Chrome $(jq_ "$R" 'd["version"]')"
for n in alice bob carol; do
  ok=$(jq_ "$R" "d['results']['$n'].get('ok')")
  others=$(jq_ "$R" "len([k for k,v in d['results']['$n'].get('decoded',{}).items() if k!='$n' and v>0])")
  [ "$ok" = True ] && [ "$others" = 2 ] && pass "$n: decoded both others, DataChannel round trip ($(jq_ "$R" "sorted(d['results']['$n'].get('pongsFrom', []))"))" \
    || fail "$n: ok=$ok decoded=$(jq_ "$R" "d['results']['$n'].get('decoded')") error=$(jq_ "$R" "d['results']['$n'].get('error')") log=$(jq_ "$R" "d['results']['$n'].get('log')")"
done
hi=$(jq_ "$R" "d['results']['carol'].get('widthHigh', 0)"); lo=$(jq_ "$R" "d['results']['carol'].get('widthLow', 0)")
[ "${hi:-0}" -gt "${lo:-0}" ] && [ "${lo:-0}" -gt 0 ] && pass "simulcast layer switch r2 ${hi}px → r0 ${lo}px" || fail "layer switch hi=$hi lo=$lo"
[ "$(cat "$W/rest_participants")" = 3 ] && pass "REST: 3 participants listed" || fail "REST participants $(cat "$W/rest_participants")"
python3 -c "import json,sys; d=json.load(open(sys.argv[1])); ps=d['participants']; sys.exit(0 if len(ps)==3 and min(p['forwarded_bytes'] for p in ps)>100000 else 1)" "$W/rest_stats.json" \
  && pass "REST: stats show forwarded media for all 3" || fail "REST stats $(head -c 300 "$W/rest_stats.json")"
[ "$(json "['delivered']" < "$W/rest_push.json")" = 3 ] && pass "REST: push delivered to 3 participants" || fail "REST push $(cat "$W/rest_push.json")"
for ev in room.started room.connection.created room.track.published room.connection.destroyed; do
  grep -q "^event: $ev" "$W/sse.txt" && pass "SSE: $ev" || fail "SSE: no $ev"
done
grep -q '"client_id":"alice"' "$W/sse.txt" && pass "SSE: events carry the token's client_id" || fail "SSE client_id"
# 部屋を消すと参加者は切れ、SSE に room.deleted
c=$(code DELETE /rooms/demo); [ "$c" = 204 ] && pass "room deleted" || fail "delete $c"
sleep 1
grep -q "^event: room.deleted" "$W/sse.txt" && pass "SSE: room.deleted" || fail "SSE: no room.deleted"
summary
