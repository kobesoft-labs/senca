#!/usr/bin/env bash
# M9: 3 台の制御面（自前の Raft）。転送・リーダーの停止・追いつき・過半数の喪失（AC-15 の一部）。監査ログのクラスター全体の読み出し。
# ノード間の挑戦応答・分断での fencing（SIGSTOP）・所有権の手放し・/readyz・排出・心拍の使用量も確かめる。
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
declare -A HTTP RAFT RTMPP RELAY UDPP
for i in 1 2 3; do HTTP[$i]=$(port); RAFT[$i]=$(port); RTMPP[$i]=$(port); RELAY[$i]=$(port); UDPP[$i]=$(port); done
for i in 1 2 3; do
  T=""; [ -n "${CLUSTER_TLS:-}" ] && T="\"tls_ca\": \"$W/ca.pem\", \"tls_cert\": \"$W/n$i.pem\", \"tls_key\": \"$W/n$i.key\","
  cat > "$W/c$i.json" <<J
{"node_id": $i, "listen": "127.0.0.1:${RAFT[$i]}", "relay_listen": "127.0.0.1:${RELAY[$i]}", "secret_file": "$W/secret", $T
 "peers": [{"id":1,"addr":"127.0.0.1:${RAFT[1]}","relay":"127.0.0.1:${RELAY[1]}","api":"http://127.0.0.1:${HTTP[1]}"},
           {"id":2,"addr":"127.0.0.1:${RAFT[2]}","relay":"127.0.0.1:${RELAY[2]}","api":"http://127.0.0.1:${HTTP[2]}"},
           {"id":3,"addr":"127.0.0.1:${RAFT[3]}","relay":"127.0.0.1:${RELAY[3]}","api":"http://127.0.0.1:${HTTP[3]}"}]}
J
done
start() {
  local i=$1
  "$NODE_BIN" --data-dir "$W/d$i" --http 127.0.0.1:${HTTP[$i]} --rtmp 127.0.0.1:${RTMPP[$i]} --public-http-url http://127.0.0.1:${HTTP[$i]} \
    --reconnect-grace 2000 --cluster-config "$W/c$i.json" --node-name n$i --egress-allow 127.0.0.0/8 --label zone=z$i --webrtc-udp 127.0.0.1:${UDPP[$i]} >>"$W/n$i.log" 2>&1 &
  PIDS[$i]=$!
}
ready() { for _ in $(seq 100); do curl -sf http://127.0.0.1:${HTTP[$1]}/readyz >/dev/null && return 0; sleep 0.2; done; return 1; }
for i in 1 2 3; do start $i; done
for i in 1 2 3; do ready $i || { fail "node $i not ready"; cat "$W/n$i.log"; summary; exit 1; }; done
pass "3 nodes ready"
# ---- ノード間の認証（挑戦応答）: 合言葉を知らない相手は、名乗っても入れない（固定の値は送られない） ----
if [ -z "${CLUSTER_TLS:-}" ]; then
  probe_auth() { python3 - "$1" "$2" <<'PY'
import os, socket, struct, sys
port, purpose = int(sys.argv[1]), int(sys.argv[2])
s = socket.create_connection(("127.0.0.1", port), timeout=5)
s.sendall(b"SNCA" + bytes([1, purpose]) + struct.pack(">Q", 2) + os.urandom(32))
ch = b""
while len(ch) < 72:
    d = s.recv(72 - len(ch))
    if not d:
        break
    ch += d
s.sendall(os.urandom(32))  # 偽の証明
try:
    rest = s.recv(16)
except OSError:
    rest = b""
print("challenge" if len(ch) == 72 else "no-challenge", "closed" if rest == b"" else "open")
PY
  }
  r=$(probe_auth ${RAFT[1]} 1); sleep 0.3
  [ "$r" = "challenge closed" ] && grep -q "cluster: rejected connection from .*合言葉が違います" "$W/n1.log" \
    && pass "raft: a peer without the cluster secret is refused (challenge-response)" || fail "raft auth probe: $r"
  r=$(probe_auth ${RELAY[1]} 2); sleep 0.3
  [ "$r" = "challenge closed" ] && grep -q "relay: connection from .* refused: .*合言葉が違います" "$W/n1.log" \
    && pass "relay: a peer without the cluster secret is refused" || fail "relay auth probe: $r"
fi
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
ok=0; for _ in $(seq 40); do c=$(curl -s -o /dev/null -w '%{http_code}' http://127.0.0.1:${HTTP[1]}/readyz); [ "$c" = 503 ] && { ok=1; break; }; sleep 0.25; done
[ $ok = 1 ] && pass "minority node is not ready (/readyz 503)" || fail "minority readyz $c"
# 戻すと書ける
start 2; start 3; ready 2; ready 3
ok=0; for _ in $(seq 40); do c=$(code 3 POST /streams -d '{"name":"healed"}'); [ "$c" = 201 ] && { ok=1; break; }; sleep 0.5; done
[ $ok = 1 ] && pass "writes resume after heal" || fail "heal write $c"
ok=0; for _ in $(seq 40); do curl -sf http://127.0.0.1:${HTTP[1]}/readyz >/dev/null && { ok=1; break; }; sleep 0.25; done
[ $ok = 1 ] && pass "node 1 is ready again after heal" || fail "node 1 readyz after heal"
sleep 1
n1=$(api 1 GET /streams | json "['items'].__len__()"); n3=$(api 3 GET /streams | json "['items'].__len__()")
[ "$n1" = 5 ] && [ "$n3" = 5 ] && pass "all nodes converge (5 streams)" || fail "converge n1=$n1 n3=$n3"
# メディア: 所有権と中継（ADR-0014）。node 2 で受けた配信を node 3・node 1 で見る → 案内せず、中継の写しから配る
"$FFMPEG" -v error -f lavfi -i testsrc2=size=320x240:rate=30 -t 10 -c:v libx264 -g 30 -f flv "$W/src.flv"
frames "$W/src.flv" v > "$W/src.v"
S=$(api 1 POST /streams -d '{"name":"media"}'); SID=$(echo "$S" | json "['id']"); KEY=$(echo "$S" | json "['ingest']['stream_key']")
"$FFMPEG" -v error -re -i "$W/src.flv" -c copy -f flv rtmp://127.0.0.1:${RTMPP[2]}/live/$KEY & PUB=$!
sleep 3
URL3=$(api 3 POST /streams/$SID/playback-url | json "['playback']['hls']")
c=$(curl -s -o /dev/null -w '%{http_code}' "$URL3")
[ "$c" = 200 ] && pass "viewer on node 3 served locally via relay (no redirect)" || fail "relay playlist on node 3: $c"
frames "$URL3" v > "$W/r3.v" &
FR=$!
# RTMP play on node 1 も中継から（署名付きキー）
PURL1=$(api 1 POST /streams/$SID/playback-url | json "['playback']['rtmp']" 2>/dev/null || true)
if [ -n "$PURL1" ] && [ "$PURL1" != None ]; then
  P1=$(echo "$PURL1" | sed "s#rtmp://[^/]*/#rtmp://127.0.0.1:${RTMPP[1]}/#")
  "$FFMPEG" -v error -i "$P1" -t 3 -c copy -f flv "$W/p1.flv" 2>/dev/null || true
  frames "$W/p1.flv" v 2>/dev/null | head -n -5 > "$W/p1.v" || true
  contiguous "$W/src.v" "$W/p1.v" 60 && pass "RTMP play on node 1 via relay = contiguous source run" || fail "RTMP play via relay ($(wc -l < "$W/p1.v" 2>/dev/null))"
fi
wait $PUB || true
wait $FR || true
contiguous "$W/src.v" "$W/r3.v" 100 && pass "HLS via relay on node 3 = contiguous source run ($(wc -l < "$W/r3.v") frames)" || fail "HLS via relay ($(wc -l < "$W/r3.v"))"
st=$(api 2 GET /nodes | python3 -c "import json,sys; d=json.load(sys.stdin); r=[n['relay'] for n in d['items'] if n['self']][0]; print(max(p['tx_aus'] for p in r['peers']) if r['peers'] else 0)")
[ "${st:-0}" -gt 100 ] && pass "owner node 2 relayed AUs ($st)" || fail "relay stats on node 2: $st"
# 字幕は所有者のノードで作る: 写しのノード（node 3）では、master.m3u8・字幕のプレイリスト・WebVTT を所有者から取り次ぐ
CP=$(port)
python3 "$ROOT/tools/interop/caption_mock.py" $CP 2> "$W/cmock.err" & CM=$!
sleep 0.5
CS=$(api 1 POST /streams -d "{\"name\":\"capt\",\"captions\":{\"asr_provider\":\"openai\",\"asr_endpoint\":\"http://127.0.0.1:$CP/v1\",\"language\":\"ja\"}}")
CSID=$(echo "$CS" | json "['id']"); CKEY=$(echo "$CS" | json "['ingest']['stream_key']")
"$FFMPEG" -v error -re -f lavfi -i testsrc2=size=320x240:rate=30 -f lavfi -i sine=frequency=500:sample_rate=48000 -t 20 \
  -c:v libx264 -g 30 -c:a aac -f flv rtmp://127.0.0.1:${RTMPP[2]}/live/$CKEY & CPUB=$!
sleep 3
CU3=$(api 3 POST /streams/$CSID/playback-url | json "['playback']['hls']"); CB3=${CU3%index.m3u8}
curl -s -o /dev/null "$CU3"
sleep 12
c=$(curl -s -o "$W/cmaster.m3u8" -w '%{http_code}' "${CB3}master.m3u8")
[ "$c" = 200 ] && grep -q 'TYPE=SUBTITLES' "$W/cmaster.m3u8" && grep -q 'index.m3u8' "$W/cmaster.m3u8" \
  && pass "captions: master.m3u8 served on the mirror node 3" || fail "captions: master on node 3: $c $(head -3 "$W/cmaster.m3u8")"
curl -s "${CB3}sub_ja.m3u8" > "$W/csub.m3u8"
ok=0; for f in $(grep '\.vtt' "$W/csub.m3u8"); do curl -s "${CB3}$f" | grep -q 'テスト' && ok=1; done
[ $ok = 1 ] && pass "captions: WebVTT cues served on the mirror node 3" || fail "captions: no cues on node 3 ($(head -c 200 "$W/csub.m3u8"))"
grep -q "mirroring stream $CSID" "$W/n3.log" && pass "captions: node 3 serves the stream from a relay mirror" || fail "captions: node 3 is not a mirror"
kill $CPUB 2>/dev/null; wait $CPUB 2>/dev/null || true; kill $CM 2>/dev/null || true
# 同じ stream を node 2 で配信し直し、node 3 の写しから見ている間に node 3 で受け直す → node 2 の送り手は切られ、node 3 の写しは本物に譲る
"$FFMPEG" -v error -re -stream_loop 2 -i "$W/src.flv" -c copy -f flv rtmp://127.0.0.1:${RTMPP[2]}/live/$KEY & PUB=$!
sleep 3
curl -s -o /dev/null "$URL3"
sleep 1
# 所有者の送り手のつなぎ直し（ClaimStream がもう一度来る）で、node 3 の写しが壊れない
before=$(grep -c "mirror of $SID stopped" "$W/n3.log" || true)
kill $PUB 2>/dev/null; wait $PUB 2>/dev/null || true
"$FFMPEG" -v error -re -stream_loop 2 -i "$W/src.flv" -c copy -f flv rtmp://127.0.0.1:${RTMPP[2]}/live/$KEY & PUB=$!
sleep 4
after=$(grep -c "mirror of $SID stopped" "$W/n3.log" || true)
pl=$(curl -s "$URL3"); echo "$pl" > "$W/pl_after_reconnect.m3u8"
[ "$before" = "$after" ] && echo "$pl" | grep -q EXTINF && ! echo "$pl" | grep -q ENDLIST \
  && pass "owner publisher reconnect keeps the node 3 mirror live" || fail "mirror broken by owner reconnect (before=$before after=$after)"
"$FFMPEG" -v error -re -i "$W/src.flv" -c copy -f flv rtmp://127.0.0.1:${RTMPP[3]}/live/$KEY & PUB3=$!
sleep 2
rc=0; wait $PUB 2>/dev/null || rc=$?
grep -q "publisher evicted" "$W/n2.log" && pass "old owner (node 2) evicted its publisher" || fail "node 2 did not evict (rc=$rc)"
grep -q "mirror of $SID stopped (stopped)" "$W/n3.log" && pass "node 3 mirror yielded to the real publisher" || fail "node 3 mirror not stopped"
sleep 1
URL1=$(api 1 POST /streams/$SID/playback-url | json "['playback']['hls']")
frames "$URL1" v | tail -n 100 > "$W/m.v"
wait $PUB3 || true
# node 1 の写しには前の所有者の分・不連続・新しい所有者の分が順に並ぶ（ライブの再生リストとして正しい）。終わりを比べる
contiguous "$W/src.v" "$W/m.v" 100 && pass "playback on node 1 relays from the new owner (node 3)" || fail "playback via new owner ($(wc -l < "$W/m.v"))"
# ---- 入力が終わったら所有権を手放す（ReleaseStream）: 止まった所有者へ案内しない（404） ----
R=$(api 1 POST /streams -d '{"name":"released"}'); RID=$(echo "$R" | json "['id']"); RKEY=$(echo "$R" | json "['ingest']['stream_key']")
"$FFMPEG" -v error -re -i "$W/src.flv" -t 3 -c copy -f flv rtmp://127.0.0.1:${RTMPP[2]}/live/$RKEY || true
ok=0; for _ in $(seq 40); do grep -q "released ownership of stream $RID" "$W/n2.log" && { ok=1; break; }; sleep 0.25; done
[ $ok = 1 ] && pass "owner node 2 released the stream after its input ended (reconnect grace)" || fail "no release on node 2"
sleep 0.5
RURL3=$(api 3 POST /streams/$RID/playback-url | json "['playback']['hls']")
c=$(curl -s -o /dev/null -w '%{http_code}' "$RURL3")
[ "$c" = 404 ] && pass "viewer on node 3 gets 404 for an ended stream (no redirect to a stale owner)" || fail "after release: $c"
# ---- グリッド（ADR-0013）: 手動の割り当て・ノードの停止での移動・戻り・strict の割り当て ----
g=$(api 1 GET /grid)
nup=$(echo "$g" | python3 -c "import json,sys; d=json.load(sys.stdin); print(sum(1 for n in d['nodes'] if n['up']))")
[ "$nup" = 3 ] && pass "grid: 3 nodes registered and up" || fail "grid nodes up: $nup"
# 心拍の使用量は実測（/proc）。配置係（リーダー）の GET /grid に出る
ok=0; r=""
for _ in $(seq 20); do
  for i in 1 2 3; do
    r=$(api $i GET /grid | python3 -c "import json,sys; d=json.load(sys.stdin); print('lead' if d['scheduler'] else 'no', all((n.get('usage') or {}).get('memory_mb',0)>0 for n in d['nodes'] if n['up']))")
    [ "$r" = "lead True" ] && { ok=1; break 2; }
  done
  sleep 0.5
done
[ $ok = 1 ] && pass "grid: heartbeats carry measured usage (memory from /proc/meminfo)" || fail "grid usage: $r"
# 元の配信（node 1 で受け続ける）
G=$(api 1 POST /streams -d '{"name":"grid-src"}'); GSID=$(echo "$G" | json "['id']"); GKEY=$(echo "$G" | json "['ingest']['stream_key']")
"$FFMPEG" -v error -re -stream_loop -1 -i "$W/src.flv" -c copy -f flv rtmp://127.0.0.1:${RTMPP[1]}/live/$GKEY & GPUB=$!
sleep 2
GURL=$(api 1 POST /streams/$GSID/playback-url -d '{"ttl_secs":3600}' | json "['playback']['rtmp']" | sed "s#rtmp://[^/]*/#rtmp://127.0.0.1:${RTMPP[1]}/#")
D=$(api 1 POST /streams -d '{"name":"grid-dst"}'); DSID=$(echo "$D" | json "['id']")
IN=$(api 2 POST /streams/$DSID/inputs -d "{\"protocol\":\"rtmp_pull\",\"url\":\"$GURL\",\"placement\":{\"node\":\"n3\",\"pin_policy\":\"prefer\",\"priority\":\"high\"}}")
IID=$(echo "$IN" | json "['id']")
placed() { api $1 GET /inputs/$IID | python3 -c "import json,sys; d=json.load(sys.stdin)['placement_status']; print(d.get('node_name') or d.get('state'))"; }
wait_placed() { local n=$1 want=$2; for _ in $(seq 60); do [ "$(placed $n)" = "$want" ] && return 0; sleep 0.5; done; return 1; }
wait_placed 1 n3 && pass "grid: input pinned to n3 (prefer) is placed on n3" || fail "grid: placed on $(placed 1)"
sleep 3
act=0; for i in 1 2 3; do s=$(api $i GET /inputs/$IID | json "['status']['state']" 2>/dev/null || echo none); [ "$s" = active ] && act=$((act+1)) && who=$i; done
[ "$act" = 1 ] && [ "$who" = 3 ] && pass "grid: exactly one node (n3) runs the input" || fail "grid: running on $act nodes (last $who)"
# n3 を落とす → 5 秒ほどで停止と見なし、prefer なので別のノードへ移る（Swarm のように縮退して動き続ける）
kill -9 ${PIDS[3]}; PIDS[3]=
t0=$(date +%s)
moved=""; for _ in $(seq 60); do p=$(placed 1); case "$p" in n1|n2) moved=$p; break;; esac; sleep 0.5; done
[ -n "$moved" ] && pass "grid: n3 down → input moved to $moved in $(( $(date +%s) - t0 )) s" || fail "grid: not moved ($(placed 1))"
mi=${moved#n}
ok=0; for _ in $(seq 30); do [ "$(api $mi GET /inputs/$IID | json "['status']['state']" 2>/dev/null)" = active ] && { ok=1; break; }; sleep 0.5; done
[ $ok = 1 ] && pass "grid: the input is active again on $moved" || fail "grid: not active on $moved"
# strict の割り当て: n2 に固定。n2 が落ちたら移さない
IN2=$(api 1 POST /streams/$DSID/inputs -d "{\"protocol\":\"rtmp_pull\",\"url\":\"$GURL\",\"enabled\":false,\"placement\":{\"node\":\"n2\",\"pin_policy\":\"strict\"}}")
IID2=$(echo "$IN2" | json "['id']")
c=$(code 1 POST /streams/$DSID/inputs -d '{"protocol":"rtmp_pull","url":"rtmp://127.0.0.1:1/a/b","placement":{"node":"no-such-node"}}')
[ "$c" = 400 ] && pass "grid: unknown node in placement is 400" || fail "grid: unknown node $c"
c=$(code 1 POST /streams/$DSID/inputs -d '{"protocol":"rtmp_pull","url":"rtmp://127.0.0.1:1/a/b","placement":{"degrade":[{"label":"x","disable":["a"]}]}}')
[ "$c" = 400 ] && pass "grid: degrade levels only for pipelines (400)" || fail "grid: degrade on input $c"
# n3 を戻す → 冷却（30 秒）の後、prefer の割り当ては n3 に戻る
start 3; ready 3 || fail "node 3 restart"
back=0; for _ in $(seq 90); do [ "$(placed 1)" = n3 ] && { back=1; break; }; sleep 1; done
[ $back = 1 ] && pass "grid: after n3 recovered, the prefer-pinned input returned to n3" || fail "grid: did not return ($(placed 1))"
st=$(api 1 GET /inputs/$IID2 | python3 -c "import json,sys; print(json.load(sys.stdin)['placement_status']['state'])")
[ "$st" = not_scheduled ] && pass "grid: disabled input has no workload" || fail "grid: disabled input state $st"
# strict の割り当て: n2 に固定して有効にする → n2 を落としても他へ移さない → n2 が戻れば n2 で動く
E2=$(api 1 GET /inputs/$IID2 -D - -o /dev/null | tr -d '\r' | awk -F': ' 'tolower($1)=="etag"{print $2}')
api 1 PATCH /inputs/$IID2 -H "If-Match: $E2" -d '{"enabled":true}' >/dev/null
placed2() { api 1 GET /inputs/$IID2 | python3 -c "import json,sys; d=json.load(sys.stdin)['placement_status']; print(d.get('node_name') or d.get('state'))"; }
ok=0; for _ in $(seq 40); do [ "$(placed2)" = n2 ] && { ok=1; break; }; sleep 0.5; done
[ $ok = 1 ] && pass "grid: strict pin placed on n2" || fail "grid: strict pin at $(placed2)"
kill -9 ${PIDS[2]}; PIDS[2]=
sleep 12
st=$(placed2)
case "$st" in unschedulable|pending) pass "grid: strict pin stays unscheduled while n2 is down ($st)";; *) fail "grid: strict pin moved to $st";; esac
start 2; ready 2 || fail "node 2 restart"
ok=0; for _ in $(seq 60); do [ "$(placed2)" = n2 ] && { ok=1; break; }; sleep 0.5; done
[ $ok = 1 ] && pass "grid: strict pin runs on n2 again after it recovered" || fail "grid: strict pin after recovery $(placed2)"
kill $GPUB 2>/dev/null || true
# ---- 分断（ADR-0013 の fencing）: n1・n2 を止める（SIGSTOP）→ 少数側の n3 は割り当てを確かめられないので、
#      pull の入力を止め、/readyz は 503。SIGCONT で戻り、入力はどれか 1 台で動き直す ----
F=$(api 1 POST /streams -d '{"name":"fence-src"}'); FSID=$(echo "$F" | json "['id']"); FKEY=$(echo "$F" | json "['ingest']['stream_key']")
"$FFMPEG" -v error -re -stream_loop -1 -i "$W/src.flv" -c copy -f flv rtmp://127.0.0.1:${RTMPP[3]}/live/$FKEY & FPUB=$!
sleep 2
FURL=$(api 3 POST /streams/$FSID/playback-url -d '{"ttl_secs":3600}' | json "['playback']['rtmp']" | sed "s#rtmp://[^/]*/#rtmp://127.0.0.1:${RTMPP[3]}/#")
FD=$(api 1 POST /streams -d '{"name":"fence-dst"}'); FDID=$(echo "$FD" | json "['id']")
FIN=$(api 1 POST /streams/$FDID/inputs -d "{\"protocol\":\"rtmp_pull\",\"url\":\"$FURL\",\"placement\":{\"node\":\"n3\",\"pin_policy\":\"prefer\"}}")
FIID=$(echo "$FIN" | json "['id']")
fstate() { api $1 GET /inputs/$FIID | json "['status']['state']" 2>/dev/null || echo none; }
ok=0; for _ in $(seq 60); do [ "$(fstate 3)" = active ] && { ok=1; break; }; sleep 0.5; done
[ $ok = 1 ] && pass "fencing: input pinned to n3 is active on n3" || fail "fencing: input not active on n3 ($(fstate 3))"
kill -STOP ${PIDS[1]} ${PIDS[2]}
t0=$(date +%s); s=""
ok=0; for _ in $(seq 60); do s=$(fstate 3); [ "$s" != active ] && grep -q "grid: fenced" "$W/n3.log" && { ok=1; break; }; sleep 0.25; done
[ $ok = 1 ] && pass "fencing: minority node n3 stopped its pulled input ($(( $(date +%s) - t0 )) s after losing the quorum)" || fail "fencing: n3 still runs the input ($s)"
c=$(curl -s -o /dev/null -w '%{http_code}' http://127.0.0.1:${HTTP[3]}/readyz); [ "$c" = 503 ] && pass "fencing: /readyz on the minority node is 503" || fail "fencing: readyz $c"
sleep 3; s=$(fstate 3)
[ "$s" != active ] && pass "fencing: the input stays stopped while partitioned" || fail "fencing: restarted while partitioned ($s)"
kill -CONT ${PIDS[1]} ${PIDS[2]}
ok=0; for _ in $(seq 60); do curl -sf http://127.0.0.1:${HTTP[3]}/readyz >/dev/null && { ok=1; break; }; sleep 0.5; done
[ $ok = 1 ] && pass "fencing: n3 is ready again after the partition healed" || fail "fencing: n3 not ready after heal"
act=0; ok=0
for _ in $(seq 80); do act=0; for i in 1 2 3; do [ "$(fstate $i)" = active ] && act=$((act+1)); done; [ $act = 1 ] && { ok=1; break; }; sleep 0.5; done
[ $ok = 1 ] && pass "fencing: the input runs again on exactly one node after heal" || fail "fencing: active on $act nodes after heal"
kill $FPUB 2>/dev/null || true
# ---- 部屋（ADR-0015）: グリッドで置く・別のノードからの案内・REST の中継・ノードの停止で移る ----
c=$(code 1 POST /rooms -d '{"id":"r-grid","settings":{"max_participants":4},"placement":{"node":"n2","pin_policy":"prefer"}}')
[ "$c" = 201 ] && pass "rooms: room created with a prefer pin to n2" || fail "rooms: create $c"
rplaced() { api $1 GET /rooms/r-grid | python3 -c "import json,sys; d=json.load(sys.stdin)['placement_status']; print(d.get('node_name') or d.get('state'))"; }
ok=0; for _ in $(seq 40); do [ "$(rplaced 1)" = n2 ] && { ok=1; break; }; sleep 0.25; done
[ $ok = 1 ] && pass "rooms: room placed on n2" || fail "rooms: placed $(rplaced 1)"
RT=$(api 3 POST /rooms/r-grid/tokens -d '{"role":"recvonly","client_id":"probe"}' | json "['token']")
r=$(python3 "$ROOT/tools/interop/ws_probe.py" "ws://127.0.0.1:${HTTP[1]}/signaling" r-grid "$RT")
echo "$r" | grep -q "\"type\": *\"redirect\"" && echo "$r" | grep -q "127.0.0.1:${HTTP[2]}/signaling" \
  && pass "rooms: signaling on n1 redirects to the hosting node n2" || fail "rooms: redirect $r"
r=$(python3 "$ROOT/tools/interop/ws_probe.py" "ws://127.0.0.1:${HTTP[2]}/signaling" r-grid "$RT")
echo "$r" | grep -q "\"type\": *\"connected\"" && pass "rooms: signaling on n2 joins the room" || fail "rooms: join on n2 $r"
c=$(code 3 GET /rooms/r-grid/participants); [ "$c" = 200 ] && pass "rooms: REST on n3 is forwarded to n2 (participants 200)" || fail "rooms: forward $c"
c=$(code 3 POST /rooms/r-grid/messages -d '{"data":{"x":1}}'); [ "$c" = 200 ] && pass "rooms: push via n3 forwarded" || fail "rooms: push forward $c"
# n2 を落とす → 部屋は別のノードへ移り、そこで入れる
kill -9 ${PIDS[2]}; PIDS[2]=
moved=""; for _ in $(seq 60); do p=$(rplaced 1); case "$p" in n1|n3) moved=$p; break;; esac; sleep 0.5; done
[ -n "$moved" ] && pass "rooms: n2 down → room moved to $moved" || fail "rooms: room not moved ($(rplaced 1))"
mi=${moved#n}
r=$(python3 "$ROOT/tools/interop/ws_probe.py" "ws://127.0.0.1:${HTTP[1]}/signaling" r-grid "$RT")
if [ "$mi" = 1 ]; then
  echo "$r" | grep -q "\"type\": *\"connected\"" && pass "rooms: participants can rejoin on the new node" || fail "rooms: rejoin $r"
else
  echo "$r" | grep -q "127.0.0.1:${HTTP[$mi]}/signaling" && pass "rooms: n1 now redirects to the new node $moved" || fail "rooms: redirect after move $r"
fi
grep -q "rooms: room r-grid hosted here" "$W/n$mi.log" && pass "rooms: $moved hosts the room" || fail "rooms: $moved did not host"
start 2; ready 2 || fail "node 2 restart"
# ---- 排出（drain）: 新しい送り手は断る（WHIP は排出中でないノードへ 307）。つながっている送り手はそのまま ----
N2ID=$(api 1 GET /nodes | python3 -c "import json,sys; print([n['id'] for n in json.load(sys.stdin)['items'] if n['name']=='n2'][0])")
DA=$(api 1 POST /streams -d '{"name":"drain-live"}'); DAID=$(echo "$DA" | json "['id']"); DAKEY=$(echo "$DA" | json "['ingest']['stream_key']")
DB=$(api 1 POST /streams -d '{"name":"drain-new"}'); DBID=$(echo "$DB" | json "['id']"); DBKEY=$(echo "$DB" | json "['ingest']['stream_key']")
"$FFMPEG" -v error -re -stream_loop -1 -i "$W/src.flv" -c copy -f flv rtmp://127.0.0.1:${RTMPP[2]}/live/$DAKEY & DPUB=$!
sleep 2
c=$(code 1 POST /nodes/$N2ID/drain); [ "$c" = 202 ] && pass "drain: node n2 drained (202)" || fail "drain: $c"
ok=0; for _ in $(seq 20); do [ "$(api 2 GET /nodes/$N2ID | json "['draining']")" = True ] && { ok=1; break; }; sleep 0.25; done
rc=0; timeout 15 "$FFMPEG" -v error -re -i "$W/src.flv" -t 2 -c copy -f flv rtmp://127.0.0.1:${RTMPP[2]}/live/$DBKEY 2>/dev/null || rc=$?
[ "$rc" != 0 ] && grep -q "rtmp publish rejected .*Draining" "$W/n2.log" && pass "drain: a new RTMP publisher on the draining node is refused" || fail "drain: RTMP publish rc=$rc"
w=$(curl -s -o /dev/null -w '%{http_code} %{redirect_url}' -X POST -H "Authorization: Bearer $DBKEY" -H 'Content-Type: application/sdp' --data-binary 'v=0' "http://127.0.0.1:${HTTP[2]}/whip/$DBID")
case "$w" in "307 http://127.0.0.1:${HTTP[1]}/whip/$DBID"|"307 http://127.0.0.1:${HTTP[3]}/whip/$DBID") pass "drain: WHIP on the draining node is redirected (307) to another node";; *) fail "drain: WHIP $w";; esac
t=$(api 2 GET /streams/$DAID | json "['live']['transport']" 2>/dev/null || echo none)
kill -0 $DPUB 2>/dev/null && [ "$t" = rtmp ] && pass "drain: the existing publisher on n2 keeps streaming" || fail "drain: existing publisher ($t)"
c=$(code 1 POST /nodes/$N2ID/undrain); [ "$c" = 202 ] || fail "undrain $c"
ok=0; for _ in $(seq 20); do [ "$(api 2 GET /nodes/$N2ID | json "['draining']")" = False ] && { ok=1; break; }; sleep 0.25; done
rc=0; timeout 15 "$FFMPEG" -v error -re -i "$W/src.flv" -t 2 -c copy -f flv rtmp://127.0.0.1:${RTMPP[2]}/live/$DBKEY 2>/dev/null || rc=$?
[ "$rc" = 0 ] && pass "drain: after undrain, n2 accepts new publishers again" || fail "undrain: publish rc=$rc"
kill $DPUB 2>/dev/null || true
# ---- 監査ログ（GET /audit）: 各ノードの audit.log を、ノード間の接続で集めて時刻の新しい順に合わせる ----
AS=$(python3 -c "import datetime;print(datetime.datetime.now(datetime.timezone.utc).strftime('%Y-%m-%dT%H:%M:%S.%f')[:-3]+'Z')")
sleep 0.05
for i in 1 2 3; do c=$(code $i POST /streams -d "{\"name\":\"audit-$i\"}"); [ "$c" = 201 ] || fail "audit: create via n$i $c"; sleep 0.05; done
api 1 GET /nodes > "$W/nodes.json"
for i in 1 2 3; do api $i GET "/audit?since=$AS&action=stream.create" > "$W/audit$i.json"; done
python3 - "$W" <<'PY' && pass "audit: every node returns the cluster-wide view, newest first, with the node that served each request" || fail "audit: cluster-wide view $(head -c 600 "$W/audit1.json")"
import json, sys
w = sys.argv[1]
ids = {n["name"]: n["id"] for n in json.load(open(f"{w}/nodes.json"))["items"]}
pages = [json.load(open(f"{w}/audit{i}.json")) for i in (1, 2, 3)]
p = pages[0]
got = [(x["node"], x["node_name"]) for x in p["items"]]
ok = got == [(ids["n3"], "n3"), (ids["n2"], "n2"), (ids["n1"], "n1")] and p["next_cursor"] is None and p["unavailable_nodes"] == []
ok &= all(x["action"] == "stream.create" and x["tenant"] == "default" and x["subject"].startswith("token:") for x in p["items"])
ok &= all(q["items"] == p["items"] for q in pages[1:])
if not ok:
    for i, q in enumerate(pages, 1):
        print(f"   n{i}:", [(x["node_name"], x["ts"]) for x in q["items"]], q["next_cursor"], q["unavailable_nodes"])
sys.exit(0 if ok else 1)
PY
N3ID=$(python3 -c "import json,sys;print([n['id'] for n in json.load(open(sys.argv[1]))['items'] if n['name']=='n3'][0])" "$W/nodes.json")
r=$(api 1 GET "/audit?since=$AS&action=stream.create&node=n2")
echo "$r" | python3 -c "import json,sys;i=json.load(sys.stdin)['items'];sys.exit(0 if [x['node_name'] for x in i]==['n2'] else 1)" \
  && pass "audit: node= restricts the read to one node (by name)" || fail "audit: node=n2 $r"
r=$(api 2 GET "/audit?since=$AS&action=stream.create&node=$N3ID")
echo "$r" | python3 -c "import json,sys;i=json.load(sys.stdin)['items'];sys.exit(0 if [x['node_name'] for x in i]==['n3'] else 1)" \
  && pass "audit: node= by id reads another node through the inter-node connection" || fail "audit: node=n3 via n2 $r"
# 全体を limit=3 でたどる = 1 ページで読んだもの（重複・取りこぼしなし、新しい順）
python3 - "http://127.0.0.1:${HTTP[2]}/api/v1" "$TOKEN" <<'PY' && pass "audit: cursor pagination over all nodes matches a single page" || fail "audit: cluster pagination"
import json, sys, urllib.request, urllib.parse
base, tok = sys.argv[1], sys.argv[2]
def get(q):
    r = urllib.request.Request(base + "/audit?" + urllib.parse.urlencode(q), headers={"Authorization": "Bearer " + tok})
    return json.load(urllib.request.urlopen(r, timeout=30))
full = get({"limit": 1000})
assert full["next_cursor"] is None and full["unavailable_nodes"] == [], full
items, cur, n = [], None, 0
while True:
    p = get({"limit": 3, **({"cursor": cur} if cur else {})})
    assert len(p["items"]) <= 3
    items += p["items"]; cur = p["next_cursor"]; n += 1
    if cur is None:
        break
key = lambda x: (x["node"], x["request_id"], x["ts"], x["action"])
assert [key(x) for x in items] == [key(x) for x in full["items"]], (len(items), len(full["items"]))
assert len(set(map(key, items))) == len(items)
assert len({x["node"] for x in items}) == 3
assert all(a["ts"] >= b["ts"] for a, b in zip(items, items[1:]))
print(f"   {len(items)} entries from 3 nodes in {n} pages")
PY
# 1 台が答えない（SIGSTOP）: ほかのノードの分を返し、答えなかったノードを unavailable_nodes に載せる。node= で指定すると 503
kill -STOP ${PIDS[3]}
r=$(api 1 GET "/audit?since=$AS&action=stream.create" --max-time 20)
c=$(code 1 GET "/audit?node=n3" --max-time 20)
kill -CONT ${PIDS[3]}
echo "$r" | python3 -c "import json,sys;p=json.load(sys.stdin);sys.exit(0 if [x['node_name'] for x in p['items']]==['n2','n1'] and p['unavailable_nodes']==[sys.argv[1]] else 1)" "$N3ID" \
  && pass "audit: a node that does not answer is listed in unavailable_nodes" || fail "audit: stopped node $r"
[ "$c" = 503 ] && pass "audit: node= for a node that does not answer is 503" || fail "audit: node=n3 stopped $c"
ok=0; for _ in $(seq 40); do curl -sf http://127.0.0.1:${HTTP[3]}/readyz >/dev/null && { ok=1; break; }; sleep 0.25; done
[ $ok = 1 ] || fail "audit: n3 ready after SIGCONT"
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
