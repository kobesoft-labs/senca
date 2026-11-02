#!/usr/bin/env bash
# M9: 3 台の制御面（自前の Raft）。転送・リーダーの停止・追いつき・過半数の喪失（AC-15 の一部）。
source "$(dirname "$0")/lib.sh"
[ -n "${NODE_BIN_PREBUILT:-}" ] || (cd "$ROOT" && cargo build -q -p senca-node)
W=$(mktemp -d); PIDS=()
cleanup() { for p in "${PIDS[@]:-}"; do kill "$p" 2>/dev/null || true; done; wait 2>/dev/null || true; [ -n "${KEEP:-}" ] && echo "kept $W" || rm -rf "$W"; }
trap cleanup EXIT
echo "0123456789abcdef-cluster-secret" > "$W/secret"
declare -A HTTP RAFT
for i in 1 2 3; do HTTP[$i]=$(port); RAFT[$i]=$(port); done
for i in 1 2 3; do
  cat > "$W/c$i.json" <<J
{"node_id": $i, "listen": "127.0.0.1:${RAFT[$i]}", "secret_file": "$W/secret",
 "peers": [{"id":1,"addr":"127.0.0.1:${RAFT[1]}"},{"id":2,"addr":"127.0.0.1:${RAFT[2]}"},{"id":3,"addr":"127.0.0.1:${RAFT[3]}"}]}
J
done
start() {
  local i=$1
  "$NODE_BIN" --data-dir "$W/d$i" --http 127.0.0.1:${HTTP[$i]} --rtmp off --cluster-config "$W/c$i.json" --node-name n$i >>"$W/n$i.log" 2>&1 &
  PIDS[$i]=$!
}
ready() { for _ in $(seq 100); do curl -sf http://127.0.0.1:${HTTP[$1]}/readyz >/dev/null && return 0; sleep 0.2; done; return 1; }
for i in 1 2 3; do start $i; done
for i in 1 2 3; do ready $i || { fail "node $i not ready"; cat "$W/n$i.log"; summary; exit 1; }; done
pass "3 nodes ready"
TOKEN=$(cat "$W/d1/control/bootstrap-admin-token")
[ ! -e "$W/d2/control/bootstrap-admin-token" ] && [ ! -e "$W/d3/control/bootstrap-admin-token" ] && pass "only one admin token created" || fail "multiple bootstrap tokens"
api() { local n=$1 m=$2 p=$3; shift 3; curl -s -X "$m" -H "Authorization: Bearer $TOKEN" -H 'Content-Type: application/json' "http://127.0.0.1:${HTTP[$n]}/api/v1$p" "$@"; }
code() { local n=$1 m=$2 p=$3; shift 3; curl -s -o /dev/null -w '%{http_code}' -X "$m" -H "Authorization: Bearer $TOKEN" -H 'Content-Type: application/json' "http://127.0.0.1:${HTTP[$n]}/api/v1$p" "$@"; }
# 全ノードで同じトークンが使える（複製された）
for i in 1 2 3; do c=$(code $i GET /streams); [ "$c" = 200 ] && pass "token valid on node $i" || fail "token on node $i: $c"; done
# どのノードからでも書ける
for i in 1 2 3; do c=$(code $i POST /streams -d "{\"name\":\"s$i\"}"); [ "$c" = 201 ] && pass "create via node $i" || fail "create via node $i: $c"; done
sleep 0.5
for i in 1 2 3; do n=$(api $i GET /streams | json "['items'].__len__()"); [ "$n" = 3 ] && pass "node $i sees 3 streams" || fail "node $i sees $n"; done
# 同じ名前は 409（適用の時点で検査）
c=$(code 2 POST /streams -d '{"name":"s1"}'); [ "$c" = 409 ] && pass "duplicate name 409 via follower" || fail "duplicate $c"
# 1 を止めても 2・3 で書ける（1 がリーダーかどうかに関わらず、過半数が残る）
kill -9 ${PIDS[1]}; PIDS[1]=; sleep 3
ok=0; for _ in $(seq 20); do c=$(code 2 POST /streams -d '{"name":"after-kill"}'); [ "$c" = 201 ] && { ok=1; break; }; sleep 0.5; done
[ $ok = 1 ] && pass "writes resume after killing node 1" || fail "writes after kill: $c"
# 1 を戻すと追いつく
start 1; ready 1 || fail "node 1 restart"
ok=0; for _ in $(seq 40); do n=$(api 1 GET /streams | json "['items'].__len__()" 2>/dev/null || echo 0); [ "$n" = 4 ] && { ok=1; break; }; sleep 0.25; done
[ $ok = 1 ] && pass "restarted node 1 caught up (4 streams)" || fail "node 1 has $n streams"
# 過半数を失うと書けない（503）、読みはできる
kill -9 ${PIDS[2]} ${PIDS[3]}; PIDS[2]=; PIDS[3]=; sleep 2
c=$(code 1 POST /streams -d '{"name":"minority"}' --max-time 10); [ "$c" = 503 ] && pass "minority cannot write (503)" || fail "minority write $c"
c=$(code 1 GET /streams); [ "$c" = 200 ] && pass "minority can still read local state" || fail "minority read $c"
# 戻すと書ける
start 2; start 3; ready 2; ready 3
ok=0; for _ in $(seq 40); do c=$(code 3 POST /streams -d '{"name":"healed"}'); [ "$c" = 201 ] && { ok=1; break; }; sleep 0.5; done
[ $ok = 1 ] && pass "writes resume after heal" || fail "heal write $c"
sleep 1
n1=$(api 1 GET /streams | json "['items'].__len__()"); n3=$(api 3 GET /streams | json "['items'].__len__()")
[ "$n1" = 5 ] && [ "$n3" = 5 ] && pass "all nodes converge (5 streams)" || fail "converge n1=$n1 n3=$n3"
summary
