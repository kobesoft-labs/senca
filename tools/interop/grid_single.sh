#!/usr/bin/env bash
# グリッド（ADR-0013）を 1 台で: 優先度による縮退・置けない理由・空きが戻ったときの復帰（冷却の後）。
# 容量を --capacity-cpu-millis で小さくして、AV1 のソフトウェア符号化（見積もり 1500 m）で溢れさせる。
source "$(dirname "$0")/lib.sh"
[ -n "${NODE_BIN_PREBUILT:-}" ] || (cd "$ROOT" && cargo build -q -p senca-node -p senca-codec-worker)
W=$(mktemp -d); NODE_PID=
cleanup() { [ -n "$NODE_PID" ] && kill "$NODE_PID" 2>/dev/null; wait 2>/dev/null || true; [ -n "${KEEP:-}" ] && echo "kept $W" || rm -rf "$W"; }
trap cleanup EXIT
CODEC_LIB_DIR=${CODEC_LIB_DIR:-$HOME/tools/codeclibs/usr/lib/x86_64-linux-gnu}
HTTP=$(port)
"$NODE_BIN" --data-dir "$W/data" --http 127.0.0.1:$HTTP --rtmp off --codec-lib-dir "$CODEC_LIB_DIR" --codec-workers 1 \
  --capacity-cpu-millis 4300 >>"$W/node.log" 2>&1 &
NODE_PID=$!
for _ in $(seq 100); do curl -sf http://127.0.0.1:$HTTP/readyz >/dev/null && break; sleep 0.1; done
TOKEN=$(cat "$W/data/control/bootstrap-admin-token")
api() { local m=$1 p=$2; shift 2; curl -s -X "$m" -H "Authorization: Bearer $TOKEN" -H 'Content-Type: application/json' "http://127.0.0.1:$HTTP/api/v1$p" "$@"; }
code() { local m=$1 p=$2; shift 2; curl -s -o /dev/null -w '%{http_code}' -X "$m" -H "Authorization: Bearer $TOKEN" -H 'Content-Type: application/json' "http://127.0.0.1:$HTTP/api/v1$p" "$@"; }
for n in src o1 o2 o3 o4; do api POST /streams -d "{\"name\":\"$n\",\"codec_profile\":\"royalty_free\",\"hls\":{\"container\":\"fmp4\"}}" > "$W/$n.json"; done
sid() { json "['id']" < "$W/$1.json"; }
for _ in $(seq 50); do api GET /grid | grep -q '"up":true' && break; sleep 0.2; done
api GET /grid | grep -q '"cpu_millis":4300' && pass "node registered with the configured capacity (4300 m)" || fail "capacity: $(api GET /grid | head -c 300)"

# A（normal）: 2 本の AV1（full 3900 m）。段 single は scale から先を止める（2100 m）
cat > "$W/a.json" <<J
{"name":"a","desired_state":"running","placement":{"priority":"normal","degrade":[{"label":"single","disable":["sc"]}]},
 "graph":{"version":1,"nodes":[
  {"type":"source","id":"s","stream":"$(sid src)","video":{"codec":"vp9"}},
  {"type":"decode","id":"d","input":"s.video"},
  {"type":"encode","id":"e1","input":"d","codec":"av1","bitrate_kbps":800},
  {"type":"scale","id":"sc","input":"d","width":320,"height":180},
  {"type":"encode","id":"e2","input":"sc","codec":"av1","bitrate_kbps":300},
  {"type":"sink","id":"k1","inputs":["e1"],"stream":"$(sid o1)"},
  {"type":"sink","id":"k2","inputs":["e2"],"stream":"$(sid o2)"}]}}
J
one() { # $1=name $2=priority $3=sink stream
cat <<J
{"name":"$1","desired_state":"running","placement":{"priority":"$2"},
 "graph":{"version":1,"nodes":[
  {"type":"source","id":"s","stream":"$(sid src)","video":{"codec":"vp9"}},
  {"type":"decode","id":"d","input":"s.video"},
  {"type":"encode","id":"e","input":"d","codec":"av1","bitrate_kbps":800},
  {"type":"sink","id":"k","inputs":["e"],"stream":"$3"}]}}
J
}
st() { api GET /pipelines/$1 | python3 -c "import json,sys; d=json.load(sys.stdin)['placement_status']; print(d['state'], d.get('level_label',''))"; }
wait_st() { for _ in $(seq ${3:-40}); do [ "$(st $1)" = "$2" ] && return 0; sleep 0.5; done; return 1; }
PA=$(api POST /pipelines --data-binary @"$W/a.json" | json "['id']")
wait_st $PA "running full" && pass "A runs at full level alone" || fail "A: $(st $PA)"
# B（high）が来る → A を single に縮退して B を置く
PB=$(one b high "$(sid o3)" | api POST /pipelines --data-binary @- | json "['id']")
wait_st $PB "running full" && pass "B (high) placed" || fail "B: $(st $PB)"
[ "$(st $PA)" = "degraded single" ] && pass "A (normal) degraded to 'single' to make room for B" || fail "A after B: $(st $PA)"
grep -q "pipeline $PA: running degraded (level 1" "$W/node.log" && pass "A restarted without the disabled branch" || fail "A not running degraded"
# C（low）は上の優先度を押しのけられない → 置けない（理由つき）
PC=$(one c low "$(sid o4)" | api POST /pipelines --data-binary @- | json "['id']")
wait_st $PC "unschedulable " && pass "C (low) is unschedulable instead of preempting higher priority" || fail "C: $(st $PC)"
api GET /pipelines/$PC | python3 -c "import json,sys; d=json.load(sys.stdin)['placement_status']; sys.exit(0 if d.get('reasons') else 1)" \
  && pass "C reports reasons" || fail "C has no reasons"
# 手で止めた装置は無い: 知らない装置は 404
c=$(code POST /nodes/$(api GET /grid | json "['nodes'][0]['id']")/devices/nvidia:0/disable); [ "$c" = 404 ] && pass "unknown device is 404" || fail "device disable $c"
# B と C を消す → 冷却（30 秒）の後に A は full へ戻る
api DELETE /pipelines/$PB >/dev/null; api DELETE /pipelines/$PC >/dev/null
sleep 5
[ "$(st $PA)" = "degraded single" ] && pass "A stays degraded during the cooldown (no flapping)" || fail "A during cooldown: $(st $PA)"
wait_st $PA "running full" 90 && pass "A restored to full after the cooldown" || fail "A not restored: $(st $PA)"
g=$(api GET /grid); echo "$g" | python3 -c "import json,sys; d=json.load(sys.stdin); n=d['nodes'][0]; sys.exit(0 if n['reserved']['cpu_millis']<=n['capacity']['cpu_millis'] else 1)" \
  && pass "reservations never exceed capacity" || fail "overcommit: $g"
summary
