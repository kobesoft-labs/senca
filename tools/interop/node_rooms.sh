#!/usr/bin/env bash
# ノードに組み込んだリアルタイムの部屋（ADR-0015）を Chrome で: REST で部屋を作り、ノードが発行した参加トークンで
# 3 つのページが `GET /signaling` から入る。互いの映像の復号、DataChannel の往復、サイマルキャストの層の選択、
# REST の参加者・統計・送信、SSE の部屋の出来事を確かめる。ノードは 0.0.0.0 で待ち、ICE の候補には
# --webrtc-public-ip の IP を載せる（ブラウザーがその IP でつながる）。
# 別に立てる小さなノードで: SSE の Last-Event-ID からの再送（再起動をまたいでも）と resync、量の上限（429・Retry-After）、
# 問題の型（UNSUPPORTED_PROTOCOL_VERSION・INPUT_TIMEOUT）、publish の権限のトークンでの WHIP、0.0.0.0 で待つときの
# ICE の候補（インターフェースのアドレス）。部屋の settings の部分更新（JSON Merge Patch）、settings.recording から
# 作るメディアパイプライン、E2EE の部屋・書き出した stream でのサーバーの処理の拒否（422）も確かめる。
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
"$NODE_BIN" --data-dir "$W/data" --http 127.0.0.1:$HP --rtmp off --webrtc-udp "0.0.0.0:$UP" --webrtc-public-ip "$IP" --public-http-url "http://127.0.0.1:$HP" >"$W/node.log" 2>&1 &
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
grep -q "ICE candidates: $IP:$UP\$" "$W/node.log" && pass "bound to 0.0.0.0, ICE candidates advertise --webrtc-public-ip $IP" || fail "ICE candidates $(grep 'ICE candidates' "$W/node.log")"

# ---- API の版、部屋の settings の部分更新、settings.recording、E2EE ----
c=$(curl -s -o "$W/v2.json" -w '%{http_code}' -H "Authorization: Bearer $TOKEN" "http://127.0.0.1:$HP/api/v2/streams")
[ "$c" = 422 ] && [ "$(json "['code']" < "$W/v2.json")" = UNSUPPORTED_PROTOCOL_VERSION ] && pass "/api/v2 is 422 UNSUPPORTED_PROTOCOL_VERSION" || fail "api v2 $c $(cat "$W/v2.json")"
api POST /rooms -d '{"id":"merge","settings":{"max_participants":5,"video_codec":"vp9","spotlight":{"number":2}}}' >/dev/null
etag_of() { api GET "$1" -D - -o /dev/null | tr -d '\r' | awk -F': ' 'tolower($1)=="etag"{print $2}'; }
r=$(api PATCH /rooms/merge -H "If-Match: $(etag_of /rooms/merge)" -d '{"settings":{"max_participants":7,"spotlight":null}}')
echo "$r" | python3 -c "import json,sys; s=json.load(sys.stdin)['settings']; sys.exit(0 if s['max_participants']==7 and s['video_codec']=='vp9' and 'spotlight' not in s else 1)" \
  && pass "PATCH /rooms settings is a merge patch (others kept, null resets to default)" || fail "merge patch $r"
c=$(code PATCH /rooms/merge -H "If-Match: $(etag_of /rooms/merge)" -d '{"settings":{"max_participants":0}}'); [ "$c" = 400 ] && pass "merged settings are validated (400)" || fail "merged invalid $c"
mps() { api GET "/media-pipelines?room=$1" | python3 -c "import json,sys; print(json.dumps([{k: m[k] for k in ('id','managed','composite','recording','export_participants')} for m in json.load(sys.stdin)['items']]))"; }
c=$(code POST /rooms -d '{"id":"rec-room","settings":{"max_participants":2,"recording":"per_participant"}}'); [ "$c" = 201 ] || fail "create rec-room $c"
m=$(mps rec-room); MID=$(echo "$m" | json "[0]['id']" 2>/dev/null || true)
echo "$m" | python3 -c "import json,sys; m=json.load(sys.stdin); sys.exit(0 if len(m)==1 and m[0]['managed'] and m[0]['recording']['participants'] and m[0]['composite'] is None else 1)" \
  && pass "settings.recording per_participant creates a managed media pipeline ($MID)" || fail "managed mp $m"
c=$(code DELETE "/media-pipelines/$MID"); [ "$c" = 409 ] && pass "the managed media pipeline cannot be deleted directly (409)" || fail "delete managed $c"
c=$(code PATCH "/media-pipelines/$MID" -H 'If-Match: "1"' -d '{"name":"x"}'); [ "$c" = 409 ] && pass "the managed media pipeline cannot be patched directly (409)" || fail "patch managed $c"
api PATCH /rooms/rec-room -H "If-Match: $(etag_of /rooms/rec-room)" -d '{"settings":{"recording":"composite"}}' >/dev/null
m=$(mps rec-room); echo "$m" | python3 -c "import json,sys; m=json.load(sys.stdin); sys.exit(0 if len(m)==1 and m[0]['composite'] and m[0]['recording']['composite'] else 1)" \
  && pass "settings.recording composite switches it to a composite recording" || fail "composite managed $m"
api PATCH /rooms/rec-room -H "If-Match: $(etag_of /rooms/rec-room)" -d '{"settings":{"recording":null}}' >/dev/null
[ "$(mps rec-room)" = "[]" ] && pass "settings.recording back to none removes it" || fail "managed mp after none $(mps rec-room)"
reason() { python3 -c "import json,sys; d=json.load(open(sys.argv[1])); print(d.get('status'), d.get('details', {}).get('reason'))" "$1" 2>/dev/null; }
api POST /rooms -d '{"id":"sec","settings":{"max_participants":2,"e2ee":true,"recording":"composite"}}' -o "$W/e1.json" >/dev/null
[ "$(reason "$W/e1.json")" = "422 e2ee" ] && pass "E2EE room with composite recording is 422" || fail "e2ee composite recording $(cat "$W/e1.json")"
c=$(code POST /rooms -d '{"id":"sec","settings":{"max_participants":2,"e2ee":true,"recording":"per_participant"}}'); [ "$c" = 201 ] && pass "E2EE room with per-participant recording is allowed" || fail "e2ee per participant $c"
api POST /media-pipelines -d '{"name":"sec-c","room":"sec","composite":{}}' -o "$W/e2.json" >/dev/null
[ "$(reason "$W/e2.json")" = "422 e2ee" ] && pass "E2EE: composite media pipeline is 422" || fail "e2ee mp composite $(cat "$W/e2.json")"
api POST /media-pipelines -d '{"name":"sec-t","room":"sec","captions":{"asr_provider":"openai","asr_endpoint":"https://asr.example/v1"}}' -o "$W/e3.json" >/dev/null
[ "$(reason "$W/e3.json")" = "422 e2ee" ] && pass "E2EE: captions media pipeline is 422" || fail "e2ee mp captions $(cat "$W/e3.json")"
api POST /rooms -d '{"id":"open","settings":{"max_participants":2}}' >/dev/null
c=$(code POST /media-pipelines -d '{"name":"open-c","room":"open","composite":{}}'); [ "$c" = 201 ] || fail "open composite mp $c"
api PATCH /rooms/open -H "If-Match: $(etag_of /rooms/open)" -d '{"settings":{"e2ee":true}}' -o "$W/e4.json" >/dev/null
[ "$(reason "$W/e4.json")" = "422 e2ee" ] && pass "E2EE: turning on e2ee with a composite media pipeline is 422" || fail "e2ee patch $(cat "$W/e4.json")"
# E2EE の部屋から書き出した stream（メディアパイプラインが metadata e2ee: "true" を付ける）
api POST /streams -d '{"name":"enc","metadata":{"e2ee":"true","room":"sec"}}' > "$W/enc.json"; ESID=$(json "['id']" < "$W/enc.json")
api POST /streams -d '{"name":"plain-dst"}' > "$W/dst.json"; DSID=$(json "['id']" < "$W/dst.json")
api GET "/streams/$ESID/snapshot" -o "$W/e5.json" >/dev/null
[ "$(reason "$W/e5.json")" = "422 e2ee" ] && pass "E2EE stream: snapshot is 422" || fail "e2ee snapshot $(cat "$W/e5.json")"
api PATCH "/streams/$ESID" -H 'If-Match: "1"' -d '{"snapshots":{"interval_secs":10,"keep":3}}' -o "$W/e6.json" >/dev/null
[ "$(reason "$W/e6.json")" = "422 e2ee" ] && pass "E2EE stream: snapshot policy is 422" || fail "e2ee snapshots $(cat "$W/e6.json")"
api PATCH "/streams/$ESID" -H 'If-Match: "1"' -d '{"captions":{"asr_provider":"openai","asr_endpoint":"https://asr.example/v1"}}' -o "$W/e7.json" >/dev/null
[ "$(reason "$W/e7.json")" = "422 e2ee" ] && pass "E2EE stream: captions are 422" || fail "e2ee captions $(cat "$W/e7.json")"
G='{"version":1,"nodes":[{"type":"source","id":"cam","stream":"'$ESID'","video":{"codec":"vp8","width":640,"height":360,"fps":30}},{"type":"decode","id":"d","input":"cam.video"},{"type":"encode","id":"e","input":"d","codec":"vp8","bitrate_kbps":500},{"type":"sink","id":"o","inputs":["e"],"stream":"'$DSID'"}]}'
api POST /pipelines -d '{"name":"e2ee-src","graph":'"$G"'}' -o "$W/e8.json" >/dev/null
[ "$(reason "$W/e8.json")" = "422 e2ee" ] && pass "E2EE stream: as a pipeline source is 422" || fail "e2ee pipeline $(cat "$W/e8.json")"
T1=$(date -u -d '+1 hour' +%Y-%m-%dT%H:%M:%SZ); T2=$(date -u -d '+2 hour' +%Y-%m-%dT%H:%M:%SZ)
api POST /schedules -d '{"stream":"'$ESID'","action":"snapshot","snapshot_interval_secs":10,"start_at":"'$T1'","end_at":"'$T2'"}' -o "$W/e9.json" >/dev/null
[ "$(reason "$W/e9.json")" = "422 e2ee" ] && pass "E2EE stream: snapshot schedule is 422" || fail "e2ee schedule $(cat "$W/e9.json")"
r=$(api PATCH "/streams/$ESID" -H 'If-Match: "1"' -d '{"metadata":{"note":"x"}}'); [ "$(echo "$r" | json "['metadata']['e2ee']")" = true ] && pass "E2EE stream: the e2ee mark survives a metadata update" || fail "e2ee mark $r"
# 試しの部屋を片付ける（ノードの容量を demo に残す）
for r in merge rec-room sec open; do code DELETE "/rooms/$r" >/dev/null; done
for _ in $(seq 100); do st=$(api GET /rooms/demo | json "['placement_status']['state']"); [ "$st" = running ] && break; sleep 0.5; done
[ "$st" = running ] && pass "demo stays placed after the extra rooms ($st)" || fail "demo placement after the extra rooms: $(api GET /rooms/demo | json "['placement_status']")"

# ---- 小さなノード: SSE の再送、問題の型、publish の権限での WHIP、0.0.0.0 の候補 ----
AHP=$(port); AUP=$(port); ARP=$(port)
astart() { "$NODE_BIN" --data-dir "$W/aux" --http 127.0.0.1:$AHP --rtmp 127.0.0.1:$ARP --webrtc-udp "0.0.0.0:$AUP" --public-http-url "http://127.0.0.1:$AHP" \
  --events-retain "$1" >>"$W/aux.log" 2>&1 & APID=$!; PIDS+=("$APID")
  for _ in $(seq 100); do curl -sf "http://127.0.0.1:$AHP/readyz" >/dev/null && break; sleep 0.1; done; }
astart 3
AT=$(cat "$W/aux/control/bootstrap-admin-token")
aapi() { local m=$1 p=$2; shift 2; curl -s -X "$m" -H "Authorization: Bearer $AT" -H 'Content-Type: application/json' "http://127.0.0.1:$AHP/api/v1$p" "$@"; }
sse() { if [ -n "$1" ]; then curl -sN --max-time "$2" -H "Authorization: Bearer $AT" -H "Last-Event-ID: $1" "http://127.0.0.1:$AHP/api/v1/events" || true
        else curl -sN --max-time "$2" -H "Authorization: Bearer $AT" "http://127.0.0.1:$AHP/api/v1/events" || true; fi; }
lastid() { grep '^id: ' | tail -1 | awk '{print $2}'; }
nev() { grep -c "^event: $1" || true; }
# 今の位置: 聞いている間に 1 つ作る
sse "" 2 > "$W/sse-a.txt" & SP=$!; sleep 0.4; aapi POST /streams -d '{"name":"a1"}' >/dev/null; wait $SP
L=$(lastid < "$W/sse-a.txt")
for n in a2 a3; do aapi POST /streams -d "{\"name\":\"$n\"}" >/dev/null; done
sse "$L" 2 > "$W/sse-b.txt"
[ -n "$L" ] && [ "$(nev stream.created < "$W/sse-b.txt")" = 2 ] && [ "$(nev resync < "$W/sse-b.txt")" = 0 ] \
  && pass "SSE: Last-Event-ID $L replays the 2 missed events (ids $(grep '^id: ' "$W/sse-b.txt" | awk '{print $2}' | tr '\n' ' '))" || fail "SSE replay from '$L': $(cat "$W/sse-b.txt")"
CUR=$(lastid < "$W/sse-b.txt")
for n in a4 a5 a6 a7; do aapi POST /streams -d "{\"name\":\"$n\"}" >/dev/null; done
sse "$CUR" 2 > "$W/sse-c.txt"
[ "$(nev resync < "$W/sse-c.txt")" = 1 ] && pass "SSE: a position older than the retained window (3 events) gets resync" || fail "SSE resync from $CUR: $(cat "$W/sse-c.txt")"
R=$(lastid < "$W/sse-c.txt")
# 再起動をまたぐ（単一ノードは制御ログの写しの後ろの分を読み直す）
kill "$APID"; wait "$APID" 2>/dev/null || true; astart 4096
sse "$R" 2 > "$W/sse-d.txt"
[ -n "$R" ] && [ "$(nev resync < "$W/sse-d.txt")" = 0 ] && pass "SSE: after a restart, Last-Event-ID $R resumes without resync" || fail "SSE after restart from '$R': $(cat "$W/sse-d.txt")"
aapi POST /streams -d '{"name":"a8"}' >/dev/null
kill "$APID"; wait "$APID" 2>/dev/null || true; astart 4096
sse "$R" 2 > "$W/sse-e.txt"
[ "$(nev stream.created < "$W/sse-e.txt")" -ge 1 ] && [ "$(nev resync < "$W/sse-e.txt")" = 0 ] && pass "SSE: an event from before the restart is replayed" || fail "SSE replay across restart: $(cat "$W/sse-e.txt")"
# キーフレームが届かない配信の静止画は 504 INPUT_TIMEOUT
aapi POST /streams -d '{"name":"nokey"}' > "$W/nk.json"; NKS=$(json "['id']" < "$W/nk.json"); NKK=$(json "['ingest']['stream_key']" < "$W/nk.json")
"$FFMPEG" -v error -re -f lavfi -i testsrc2=size=320x180:rate=30 -t 12 -c:v libx264 -g 30 -bsf:v "noise=drop=key" -f flv "rtmp://127.0.0.1:$ARP/live/$NKK" >/dev/null 2>&1 & FFP=$!; PIDS+=("$FFP")
sleep 2
c=$(aapi GET "/streams/$NKS/snapshot" -o "$W/nk-snap.json" -w '%{http_code}')
[ "$c" = 504 ] && [ "$(json "['code']" < "$W/nk-snap.json")" = INPUT_TIMEOUT ] && pass "snapshot of a live stream without a keyframe is 504 INPUT_TIMEOUT" || fail "input timeout $c $(cat "$W/nk-snap.json")"
kill "$FFP" 2>/dev/null || true
# publish の権限のトークンで WHIP（publish 鍵の代わり）。0.0.0.0 で待つときの候補はインターフェースのアドレス
aapi POST /streams -d '{"name":"whip"}' > "$W/wh.json"; WHS=$(json "['id']" < "$W/wh.json")
PUBT=$(aapi POST /tokens -d '{"name":"pub","scopes":["publish"]}' | json "['token']")
RDT=$(aapi POST /tokens -d '{"name":"rd","scopes":["stream:read"]}' | json "['token']")
FP=$(python3 -c "print(':'.join(['AB']*32))")
printf 'v=0\r\no=- 1 2 IN IP4 127.0.0.1\r\ns=-\r\nt=0 0\r\na=group:BUNDLE 0\r\na=ice-ufrag:abcd\r\na=ice-pwd:aaaaaaaaaaaaaaaaaaaaaaaa\r\na=fingerprint:sha-256 %s\r\na=setup:actpass\r\nm=audio 9 UDP/TLS/RTP/SAVPF 111\r\nc=IN IP4 0.0.0.0\r\na=mid:0\r\na=sendonly\r\na=rtcp-mux\r\na=rtpmap:111 opus/48000/2\r\n' "$FP" > "$W/offer.sdp"
whip() { curl -s -o "$W/answer.sdp" -w '%{http_code}' -X POST -H "Authorization: Bearer $1" -H 'Content-Type: application/sdp' --data-binary @"$W/offer.sdp" "http://127.0.0.1:$AHP/whip/$WHS"; }
c=$(whip "$RDT"); [ "$c" = 401 ] && pass "WHIP with a token without publish is 401" || fail "whip read token $c"
c=$(whip "$PUBT"); [ "$c" = 201 ] && pass "WHIP with a publish-scoped API token is 201" || fail "whip publish token $c"
cands=$(grep -a 'a=candidate:' "$W/answer.sdp" | awk '{print $5}' | sort -u | tr '\n' ' ')
[ -n "$cands" ] && ! echo "$cands" | grep -qw '0.0.0.0' && echo "$cands" | grep -qw "$IP" \
  && pass "bound to 0.0.0.0 without --webrtc-public-ip: candidates are the interface addresses ($cands)" || fail "auto candidates '$cands'"
kill "$APID"; wait "$APID" 2>/dev/null || true

# ---- 量の上限（429・Retry-After）----
RHP=$(port)
"$NODE_BIN" --data-dir "$W/rl" --http 127.0.0.1:$RHP --rtmp off --webrtc-udp "127.0.0.1:$(port)" --public-http-url "http://127.0.0.1:$RHP" \
  --rate-limit-token 2 --rate-limit-ip 4 --rate-limit-connect 1 --rate-limit-auth-failures 3 >"$W/rl.log" 2>&1 & PIDS+=("$!")
for _ in $(seq 100); do curl -sf "http://127.0.0.1:$RHP/readyz" >/dev/null && break; sleep 0.1; done
RT=$(cat "$W/rl/control/bootstrap-admin-token")
burst() { local i=0; for t in "$@"; do i=$((i+1)); curl -s -D "$W/rl-h$i" -o "$W/rl-b$i" -H "Authorization: Bearer $t" "http://127.0.0.1:$RHP/api/v1/streams" >/dev/null; done; }
first429() { local i; for i in $(seq "$1"); do grep -q '^HTTP/1.1 429' "$W/rl-h$i" && { echo "$i"; return; }; done; }
codes() { for i in $(seq "$1"); do head -1 "$W/rl-h$i" | awk '{print $2}'; done | tr '\n' ' '; }
burst "$RT" "$RT" "$RT" "$RT" "$RT" "$RT" "$RT" "$RT"
k=$(first429 8)
if [ -n "$k" ]; then
  ra=$(tr -d '\r' < "$W/rl-h$k" | awk -F': ' 'tolower($1)=="retry-after"{print $2}')
  [ "$(json "['code']" < "$W/rl-b$k")" = RATE_LIMITED ] && [ "$(json "['details']['limit']" < "$W/rl-b$k")" = token ] && [ "${ra:-0}" -ge 1 ] \
    && pass "per-token limit: 429 RATE_LIMITED with Retry-After $ra ($(codes 8))" || fail "token 429 $(cat "$W/rl-b$k")"
else fail "no 429 for the token limit ($(codes 8))"; fi
sleep 3
T2=$(curl -s -X POST -H "Authorization: Bearer $RT" -H 'Content-Type: application/json' "http://127.0.0.1:$RHP/api/v1/tokens" -d '{"name":"t2","scopes":["stream:read"]}' | json "['token']")
T3=$(curl -s -X POST -H "Authorization: Bearer $RT" -H 'Content-Type: application/json' "http://127.0.0.1:$RHP/api/v1/tokens" -d '{"name":"t3","scopes":["stream:read"]}' | json "['token']")
sleep 3
burst "$RT" "$T2" "$T3" "$RT" "$T2" "$T3" "$RT" "$T2" "$T3" "$RT"
k=$(first429 10)
[ -n "$k" ] && [ "$(json "['details']['limit']" < "$W/rl-b$k")" = ip ] && pass "per-IP limit across tokens: 429 with limit ip ($(codes 10))" || fail "ip limit ($(codes 10)) $( [ -n "$k" ] && cat "$W/rl-b$k")"
sleep 3
c=$(curl -s -o /dev/null -w '%{http_code}' -H "Authorization: Bearer $RT" "http://127.0.0.1:$RHP/api/v1/streams"); [ "$c" = 200 ] && pass "the limits refill" || fail "after refill $c"
sc=$(for i in 1 2 3 4; do curl -s -o /dev/null -w '%{http_code} ' -H 'Upgrade: websocket' -H 'Connection: Upgrade' "http://127.0.0.1:$RHP/signaling"; done)
echo "$sc" | grep -q 429 && pass "signaling connects per IP are limited ($sc)" || fail "signaling limit ($sc)"
ac=$(for i in 1 2 3 4; do curl -s -o /dev/null -w '%{http_code} ' -H "Authorization: Bearer senca_wrong$i" "http://127.0.0.1:$RHP/api/v1/streams"; done)
c=$(curl -s -o "$W/rl-af.json" -w '%{http_code}' -H "Authorization: Bearer $RT" "http://127.0.0.1:$RHP/api/v1/streams")
[ "$ac" = "401 401 401 429 " ] && [ "$c" = 429 ] && [ "$(json "['details']['limit']" < "$W/rl-af.json")" = auth_failures ] \
  && pass "auth failures per IP: 3 × 401, then 429 even with a valid token" || fail "auth failures ($ac) then $c $(cat "$W/rl-af.json")"

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
grep -q "^event: room.stopped" "$W/sse.txt" && pass "SSE: room.stopped after delete" || fail "SSE: no room.stopped"
summary
