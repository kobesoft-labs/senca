#!/usr/bin/env bash
# M9: 3 台の制御面（自前の Raft）。転送・リーダーの停止・追いつき・過半数の喪失（AC-15 の一部）。
source "$(dirname "$0")/lib.sh"
[ -n "${NODE_BIN_PREBUILT:-}" ] || (cd "$ROOT" && cargo build -q -p senca-node)
W=$(mktemp -d); PIDS=()
cleanup() { for p in "${PIDS[@]:-}"; do kill "$p" 2>/dev/null || true; done; wait 2>/dev/null || true; [ -n "${KEEP:-}" ] && echo "kept $W" || rm -rf "$W"; }
trap cleanup EXIT
echo "0123456789abcdef-cluster-secret" > "$W/secret"
# CLUSTER_TLS=1 でノード間を mTLS にする（クラスタの認証局、SAN に senca-cluster）
TLSCFG=""
if [ -n "${CLUSTER_TLS:-}" ]; then
  mkca() { openssl req -x509 -newkey ec -pkeyopt ec_paramgen_curve:P-256 -nodes -keyout "$W/$1.key" -out "$W/$1.pem" -days 2 -subj "/CN=$1" 2>/dev/null; }
  mkcert() { # $1=name $2=ca
    openssl req -newkey ec -pkeyopt ec_paramgen_curve:P-256 -nodes -keyout "$W/$1.key" -out "$W/$1.csr" -subj "/CN=$1" 2>/dev/null
    printf "subjectAltName=DNS:senca-cluster\nbasicConstraints=CA:FALSE\nextendedKeyUsage=serverAuth,clientAuth\n" > "$W/$1.ext"
    openssl x509 -req -in "$W/$1.csr" -CA "$W/$2.pem" -CAkey "$W/$2.key" -CAcreateserial -out "$W/$1.pem" -days 2 -extfile "$W/$1.ext" 2>/dev/null
  }
  mkca ca; mkca evilca
  for i in 1 2 3; do mkcert n$i ca; done
  mkcert intruder evilca
fi
declare -A HTTP RAFT RTMPP
for i in 1 2 3; do HTTP[$i]=$(port); RAFT[$i]=$(port); RTMPP[$i]=$(port); done
for i in 1 2 3; do
  T=""; [ -n "${CLUSTER_TLS:-}" ] && T="\"tls_ca\": \"$W/ca.pem\", \"tls_cert\": \"$W/n$i.pem\", \"tls_key\": \"$W/n$i.key\","
  cat > "$W/c$i.json" <<J
{"node_id": $i, "listen": "127.0.0.1:${RAFT[$i]}", "secret_file": "$W/secret", $T
 "peers": [{"id":1,"addr":"127.0.0.1:${RAFT[1]}","api":"http://127.0.0.1:${HTTP[1]}"},
           {"id":2,"addr":"127.0.0.1:${RAFT[2]}","api":"http://127.0.0.1:${HTTP[2]}"},
           {"id":3,"addr":"127.0.0.1:${RAFT[3]}","api":"http://127.0.0.1:${HTTP[3]}"}]}
J
done
start() {
  local i=$1
  "$NODE_BIN" --data-dir "$W/d$i" --http 127.0.0.1:${HTTP[$i]} --rtmp 127.0.0.1:${RTMPP[$i]} --public-http-url http://127.0.0.1:${HTTP[$i]} \
    --reconnect-grace 2000 --cluster-config "$W/c$i.json" --node-name n$i >>"$W/n$i.log" 2>&1 &
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
# メディア: 所有権と案内（node 2 で受けた配信を node 3 で見る → 307 で node 2 へ）
"$FFMPEG" -v error -f lavfi -i testsrc2=size=320x240:rate=30 -t 8 -c:v libx264 -g 30 -f flv "$W/src.flv"
frames "$W/src.flv" v > "$W/src.v"
S=$(api 1 POST /streams -d '{"name":"media"}'); SID=$(echo "$S" | json "['id']"); KEY=$(echo "$S" | json "['ingest']['stream_key']")
"$FFMPEG" -v error -re -i "$W/src.flv" -c copy -f flv rtmp://127.0.0.1:${RTMPP[2]}/live/$KEY & PUB=$!
sleep 3
URL3=$(api 3 POST /streams/$SID/playback-url | json "['playback']['hls']")
loc=$(curl -s -o /dev/null -w '%{http_code} %{redirect_url}' "$URL3")
case "$loc" in "307 http://127.0.0.1:${HTTP[2]}/"*) pass "viewer on node 3 redirected to owner node 2";; *) fail "redirect: $loc";; esac
# 同じ stream を node 3 で受け直す → node 2 の送り手は切られる（fencing）
"$FFMPEG" -v error -re -i "$W/src.flv" -c copy -f flv rtmp://127.0.0.1:${RTMPP[3]}/live/$KEY & PUB3=$!
sleep 2
rc=0; wait $PUB 2>/dev/null || rc=$?
grep -q "publisher evicted" "$W/n2.log" && pass "old owner (node 2) evicted its publisher" || fail "node 2 did not evict (rc=$rc)"
wait $PUB3 || true; sleep 3
URL1=$(api 1 POST /streams/$SID/playback-url | json "['playback']['hls']")
frames "$URL1" v > "$W/m.v"
contiguous "$W/src.v" "$W/m.v" 100 && pass "playback via node 1 follows redirect to new owner (node 3)" || fail "playback via redirect ($(wc -l < "$W/m.v"))"
if [ -n "${CLUSTER_TLS:-}" ]; then
  # 別の認証局の証明書を持つノードは、合言葉を知っていても入れない
  python3 - "$W" <<'PY'
import json,sys
w=sys.argv[1]; c=json.load(open(f"{w}/c1.json")); c["tls_cert"]=f"{w}/intruder.pem"; c["tls_key"]=f"{w}/intruder.key"; c["node_id"]=1
json.dump(c,open(f"{w}/evil.json","w"))
PY
  EH=$(port)
  "$NODE_BIN" --data-dir "$W/evil" --http 127.0.0.1:$EH --rtmp off --cluster-config "$W/evil.json" >>"$W/evil.log" 2>&1 & EP=$!
  sleep 3
  grep -q "mTLS from .* refused\|mTLS .* failed" "$W"/n*.log "$W/evil.log" && pass "node with a certificate from another CA is refused" || fail "intruder not refused"
  kill $EP 2>/dev/null || true
fi
summary
