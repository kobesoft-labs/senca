#!/usr/bin/env bash
# 部屋のメディアパイプライン（ADR-0016）を Chrome で: 2 人が部屋に入り、メディアパイプラインが
# - 参加者ごとの stream へ書き出す（再符号化しない。HLS で復号できる）
# - 2 人を並べた合成（AV1）の stream を作る
# - 参加者ごと・合成を録画する（capture）。録画は MP4 に書き出せる（concatenation）
# - 合成を RTMP で別の stream へ送る（live connector）
# ことを確かめる。
#
# 使い方: tools/interop/node_media_pipelines.sh（KEEP=1 で作業場所を残す）
set -euo pipefail
source "$(dirname "$0")/lib.sh"
IMG=${BROWSER_IMAGE:-ov-chrome:latest}
if ! docker image inspect "$IMG" >/dev/null 2>&1; then echo "SKIP: Docker image $IMG がない"; exit 0; fi
CODEC_LIB_DIR=${CODEC_LIB_DIR:-$HOME/tools/codeclibs/usr/lib/x86_64-linux-gnu}
[ -n "${NODE_BIN_PREBUILT:-}" ] || (cd "$ROOT" && cargo build -q -p senca-node -p senca-codec-worker)
mkdir -p "$HOME/.cache"
W=$(mktemp -d -p "$HOME/.cache" senca-node-mp.XXXXXX)
PIDS=(); C="senca-node-mp-$$"
cleanup() {
  for p in "${PIDS[@]}"; do kill "$p" 2>/dev/null || true; done
  docker stop -t 2 "$C" >/dev/null 2>&1 || true
  wait 2>/dev/null || true
  if [ -n "${KEEP:-}" ]; then echo "kept $W"; else rm -rf "$W"; fi
}
trap cleanup EXIT
IP=${HOST_IP:-$(hostname -I | awk '{print $1}')}
HP=$(port); UP=$(port); PP=$(port); RP=$(port)
"$NODE_BIN" --data-dir "$W/data" --http 127.0.0.1:$HP --rtmp 127.0.0.1:$RP --webrtc-udp "$IP:$UP" --public-http-url "http://127.0.0.1:$HP" \
  --public-rtmp-url "rtmp://127.0.0.1:$RP" --codec-lib-dir "$CODEC_LIB_DIR" --codec-workers 2 --egress-allow 127.0.0.0/8 --reconnect-grace 2000 >"$W/node.log" 2>&1 &
PIDS+=("$!")
for _ in $(seq 100); do curl -sf "http://127.0.0.1:$HP/readyz" >/dev/null && break; sleep 0.1; done
TOKEN=$(cat "$W/data/control/bootstrap-admin-token")
api() { local m=$1 p=$2; shift 2; curl -s -X "$m" -H "Authorization: Bearer $TOKEN" -H 'Content-Type: application/json' "http://127.0.0.1:$HP/api/v1$p" "$@"; }
code() { local m=$1 p=$2; shift 2; curl -s -o /dev/null -w '%{http_code}' -X "$m" -H "Authorization: Bearer $TOKEN" -H 'Content-Type: application/json' "http://127.0.0.1:$HP/api/v1$p" "$@"; }

api POST /rooms -d '{"id":"meet","settings":{"max_participants":8,"video_codec":"vp8"}}' >/dev/null
# 送り出しの先（同じノードの別の stream。強化 RTMP で AV1 を受ける）
api POST /streams -d '{"name":"connector-out","codec_profile":"royalty_free","hls":{"container":"fmp4"}}' > "$W/out.json"
OUTKEY=$(json "['ingest']['stream_key']" < "$W/out.json"); OUTSID=$(json "['id']" < "$W/out.json")
# 不正なもの
c=$(code POST /media-pipelines -d '{"name":"bad","room":"meet","egress":[{"protocol":"rtmp_push","url":"rtmp://127.0.0.1:1/a/b"}]}'); [ "$c" = 400 ] && pass "egress without composite is 400" || fail "egress without composite $c"
c=$(code POST /media-pipelines -d '{"name":"bad","room":"nope"}'); [ "$c" = 400 ] && pass "unknown room is 400" || fail "unknown room $c"
c=$(code POST /media-pipelines -d '{"name":"bad","room":"meet","composite":{"width":641}}'); [ "$c" = 400 ] && pass "odd composite width is 400" || fail "composite width $c"
cat > "$W/mp.json" <<J
{"name":"mtg","room":"meet",
 "composite":{"layout":"grid","width":640,"height":360,"fps":30,"video_codec":"av1","bitrate_kbps":800},
 "recording":{"participants":true,"composite":true},
 "egress":[{"protocol":"rtmp_push","url":"rtmp://127.0.0.1:$RP/live/$OUTKEY"}]}
J
c=$(api POST /media-pipelines --data-binary @"$W/mp.json" -o "$W/mp-created.json" -w '%{http_code}')
[ "$c" = 201 ] && pass "media pipeline created" || fail "create media pipeline $c $(cat "$W/mp-created.json")"
MP=$(json "['id']" < "$W/mp-created.json"); CSID=$(json "['artifacts']['composite']['stream']" < "$W/mp-created.json")

for n in alice bob; do api POST /rooms/meet/tokens -d "{\"role\":\"sendrecv\",\"client_id\":\"$n\"}" > "$W/tok-$n.json"; done
SIG=$(json "['signaling_url']" < "$W/tok-alice.json")
python3 - "$ROOT/crates/senca-rooms/examples/rooms_server.rs" "$W/www/index.html" <<'PY'
import re, sys, os
src = open(sys.argv[1]).read()
page = re.search(r'const PAGE: &str = r#"(.*?)"#;', src, re.S).group(1)
page = page.replace("const tok = await (await fetch(`/token?room=${room}&name=${name}`)).json();", "const tok = { token: q.get('token') };")
page = page.replace("ws = new WebSocket(`ws://${location.host}/signaling`, 'senca.rooms.v1');", "ws = new WebSocket(q.get('ws'), 'senca.rooms.v1');")
page = page.replace("client_id: name, ", "")
# 確かめ終わってからも hold 秒は部屋に残る（書き出し・合成・録画を確かめる間）
page = page.replace("Date.now() - doneAt > 3000", "Date.now() - doneAt > Number(q.get('hold') || '3') * 1000")
assert "q.get('hold')" in page
os.makedirs(os.path.dirname(sys.argv[2]), exist_ok=True)
open(sys.argv[2], "w").write(page)
PY
(cd "$W/www" && exec python3 -m http.server $PP --bind 127.0.0.1 >/dev/null 2>&1) & PIDS+=("$!")
python3 - "$ROOT/tools/interop/rooms.sh" "$W/drive.cjs" <<'PY'
import re, sys
s = open(sys.argv[1]).read()
open(sys.argv[2], "w").write(re.search(r"cat > \"\$W/drive.cjs\" <<'EOF'\n(.*?)\nEOF\n", s, re.S).group(1))
PY
enc() { python3 -c "import urllib.parse,sys; print(urllib.parse.quote(sys.argv[1], safe=''))" "$1"; }
Q1="room=meet&name=alice&simulcast=1&peers=1&secs=60&hold=28&token=$(enc "$(json "['token']" < "$W/tok-alice.json")")&ws=$(enc "$SIG")"
Q2="room=meet&name=bob&peers=1&secs=60&hold=28&token=$(enc "$(json "['token']" < "$W/tok-bob.json")")&ws=$(enc "$SIG")"
# 参加している間に、書き出し・合成・送り出しを確かめる
(
  for _ in $(seq 120); do
    n=$(api GET /media-pipelines/$MP | python3 -c "import json,sys; print(len(json.load(sys.stdin)['artifacts']['participant_streams']))" 2>/dev/null || echo 0)
    [ "$n" = 2 ] && break; sleep 0.5
  done
  echo "$n" > "$W/n_parts"
  api GET /media-pipelines/$MP > "$W/mp.json.live"
  # 書き出した参加者の stream と合成が流れる
  for _ in $(seq 60); do
    l=$(api GET /streams/$CSID | json "['observed_state']" 2>/dev/null || echo none); [ "$l" = live ] && break; sleep 0.5
  done
  echo "$l" > "$W/composite_state"
  sleep 6
  P1=$(python3 -c "import json,sys; print(json.load(open(sys.argv[1]))['artifacts']['participant_streams'][0]['stream'])" "$W/mp.json.live")
  api GET /streams/$P1 > "$W/p1.json"
  U1=$(api POST /streams/$P1/playback-url | json "['playback']['hls']")
  curl -s "$U1" > "$W/p1.m3u8"
  { "$FFMPEG" -v error -t 4 -i "$U1" -map 0:v -f framemd5 - 2>/dev/null || true; } | grep -vc "^#" > "$W/part_frames" || true
  UC=$(api POST /streams/$CSID/playback-url | json "['playback']['hls']")
  { "$FFMPEG" -v error -t 4 -i "$UC" -map 0:v -f framemd5 - 2>/dev/null || true; } | grep -vc "^#" > "$W/comp_frames" || true
  "$FFPROBE" -v error -select_streams v:0 -show_entries stream=codec_name,width,height -of csv=p=0 "$UC" > "$W/comp_probe" 2>/dev/null || true
  api GET /streams/$OUTSID | json "['observed_state']" > "$W/connector_state" 2>/dev/null || true
) & CHECKP=$!
timeout 150 docker run --rm --name "$C" --network host -v "$W:$W" "$IMG" bash -c \
  "node '$W/drive.cjs' 'http://127.0.0.1:$PP' '$W/result.json' 90000 '$Q1' '$Q2'; chown $(id -u):$(id -g) '$W/result.json'" >"$W/browser.log" 2>&1 || true
wait $CHECKP || true
R="$W/result.json"
jq_() { python3 -I -c "import json,sys;d=json.load(open(sys.argv[1]));print(eval(sys.argv[2]))" "$1" "$2"; }
[ -s "$R" ] && [ "$(jq_ "$R" "d['results']['alice'].get('ok')")" = True ] && [ "$(jq_ "$R" "d['results']['bob'].get('ok')")" = True ] \
  && pass "both participants exchanged media in the room" || fail "browsers: $(head -c 400 "$R" 2>/dev/null)"
[ "$(cat "$W/n_parts")" = 2 ] && pass "stream pipeline: one stream per participant" || fail "participant streams $(cat "$W/n_parts")"
n=$(cat "$W/part_frames" 2>/dev/null || echo 0); [ "${n:-0}" -ge 30 ] && pass "participant stream decodes over HLS ($n frames, no re-encoding)" || fail "participant HLS frames $n"
[ "$(cat "$W/composite_state")" = live ] && pass "composite stream is live" || fail "composite state $(cat "$W/composite_state")"
n=$(cat "$W/comp_frames" 2>/dev/null || echo 0); [ "${n:-0}" -ge 30 ] && pass "composite decodes over HLS ($n frames, $(cat "$W/comp_probe"))" || fail "composite frames $n"
grep -q "^av1,640,360" "$W/comp_probe" && pass "composite is AV1 640x360" || fail "composite probe $(cat "$W/comp_probe")"
[ "$(cat "$W/connector_state" 2>/dev/null)" = live ] && pass "live connector: composite pushed over RTMP into another stream" || fail "connector state $(cat "$W/connector_state" 2>/dev/null)"
grep -q "composite with 2 participants" "$W/node.log" && pass "composite graph includes both participants" || fail "composite members"
# 参加者が去った後: 録画が確定する
sleep 6
python3 - "$HP" "$TOKEN" "$MP" "$CSID" > "$W/recs.txt" <<'PY'
import json, sys, urllib.request, time
hp, tok, mp, csid = sys.argv[1:5]
def get(p):
    r = urllib.request.Request(f"http://127.0.0.1:{hp}/api/v1{p}", headers={"Authorization": f"Bearer {tok}"})
    return json.load(urllib.request.urlopen(r))
m = get(f"/media-pipelines/{mp}")
streams = [p["stream"] for p in m["artifacts"]["participant_streams"]] + [csid]
for _ in range(40):
    recs = [r for r in get("/recordings?limit=100")["items"] if r["stream"] in streams]
    if len(recs) >= 3 and all(r["state"] in ("finalized", "interrupted") for r in recs):
        break
    time.sleep(0.5)
print(json.dumps([(r["id"], r["stream"] == csid, r["state"]) for r in recs]))
PY
nrec=$(python3 -c "import json,sys; print(len(json.load(open(sys.argv[1]))))" "$W/recs.txt")
[ "$nrec" -ge 3 ] && pass "capture: recordings for both participants and the composite ($nrec)" || fail "recordings $(cat "$W/recs.txt")"
RC=$(python3 -c "import json,sys; print([r[0] for r in json.load(open(sys.argv[1])) if r[1]][0])" "$W/recs.txt" 2>/dev/null || true)
if [ -n "$RC" ]; then
  curl -s -H "Authorization: Bearer $TOKEN" -o "$W/comp.mp4" "http://127.0.0.1:$HP/api/v1/recordings/$RC/export.mp4"
  n=$( { "$FFMPEG" -v error -i "$W/comp.mp4" -map 0:v -f framemd5 - 2>/dev/null || true; } | grep -vc "^#" || true)
  [ "${n:-0}" -ge 60 ] && pass "concatenation: composite recording exports to one MP4 ($n frames)" || fail "composite export frames $n"
else
  fail "no composite recording"
fi
# 消す: 合成のパイプライン・送り出しが片付く（書き出した stream と録画は残る）
c=$(code DELETE /media-pipelines/$MP); [ "$c" = 204 ] && pass "media pipeline deleted" || fail "delete $c"
PID=$(json "['artifacts']['composite']['pipeline']" < "$W/mp-created.json")
[ "$(code GET /pipelines/$PID)" = 404 ] && pass "composite pipeline removed with it" || fail "composite pipeline still there"
[ "$(code GET /streams/$CSID)" = 200 ] && pass "exported streams are kept as artifacts" || fail "composite stream gone"
summary
