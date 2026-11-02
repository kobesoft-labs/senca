#!/usr/bin/env bash
# STUN・TURN・ICE の相互接続試験（coturn の Docker イメージ coturn/coturn を相手にする）。
#
#  A. senca の STUN クライアント（senca-stun examples/stun_client）→ coturn の STUN
#  B. coturn の turnutils_stunclient → senca の STUN 応答器（examples/stun_server）
#  C. senca の TURN クライアント（senca-turn examples/turn_relay_check）→ coturn
#     Allocate（長期資格情報）・CreatePermission・Send/Data・ChannelBind/ChannelData・Refresh(0)。
#     誤ったパスワードは拒否。coturn の REST 方式（use-auth-secret）の一時資格情報でも通ること。
#  D. coturn の turnutils_uclient → senca の TURN サーバー（examples/turn_server）
#     channel・Send indication・relay どうし（-y）、REST 方式（-W）、誤った秘密は拒否。
#  E. senca の TURN クライアント → senca の TURN サーバー（自分どうしの確認）
#  F. 2 つの senca の ICE エージェント（senca-ice examples/ice_pair）を本物の UDP でつなぐ
#     host どうし・ICE-lite・coturn の relayed 候補だけ・senca の TURN の relayed 候補だけ。
#
# 環境変数: CARGO（既定 cargo）、CARGO_TARGET_DIR、ICE_BIN_DIR（指定すればビルドせず、そこにある
#           examples を使う）、COTURN_IMAGE（既定 coturn/coturn）。
# ポートは 46000〜46999 を使う。起動したコンテナだけを止める。
set -euo pipefail

source "$(dirname "$0")/lib.sh"
CARGO=${CARGO:-cargo}
IMAGE=${COTURN_IMAGE:-coturn/coturn}

WORK=$(mktemp -d)
PIDS=()
CONTAINERS=()
cleanup() {
    for p in "${PIDS[@]}"; do kill "$p" 2>/dev/null || true; done
    for c in "${CONTAINERS[@]}"; do docker stop -t 1 "$c" >/dev/null 2>&1 || true; done
    rm -rf "$WORK"
}
trap cleanup EXIT

if [[ -z "${ICE_BIN_DIR:-}" ]]; then
    (cd "$ROOT" && "$CARGO" build -q -p senca-stun -p senca-turn -p senca-ice --examples)
    ICE_BIN_DIR="$TARGET/debug/examples"
fi
STUN_SERVER="$ICE_BIN_DIR/stun_server"
STUN_CLIENT="$ICE_BIN_DIR/stun_client"
TURN_SERVER="$ICE_BIN_DIR/turn_server"
TURN_CHECK="$ICE_BIN_DIR/turn_relay_check"
ICE_PAIR="$ICE_BIN_DIR/ice_pair"
for b in "$STUN_SERVER" "$STUN_CLIENT" "$TURN_SERVER" "$TURN_CHECK" "$ICE_PAIR"; do
    [[ -x "$b" ]] || { echo "FAIL: $b not built"; exit 1; }
done
docker image inspect "$IMAGE" >/dev/null 2>&1 || { echo "FAIL: docker image $IMAGE not found"; exit 1; }

# 46000 + 100 刻みの帯を乱数で選ぶ（同時実行の衝突を避ける）。帯の中で
#   +0..+9 待ち受け、+10..+19 peer、+50..+99 relay ポート。
BASE=$((46000 + (RANDOM % 9) * 100))
COTURN_PORT=$((BASE + 1))
SENCA_STUN_PORT=$((BASE + 2))
SENCA_TURN_PORT=$((BASE + 3))
SENCA_TURN_REST_PORT=$((BASE + 4))
COTURN_REST_PORT=$((BASE + 5))
PEER_PORT=$((BASE + 10))
SECRET=interop-secret-$RANDOM

# coturn を起動する（listening-port, min-port, max-port, 追加引数...）。
start_coturn() {
    local port=$1 min=$2 max=$3
    shift 3
    local id
    id=$(docker run --rm -d --network host "$IMAGE" -n --listening-ip=127.0.0.1 --listening-port="$port" \
        --relay-ip=127.0.0.1 --min-port="$min" --max-port="$max" --realm=r --no-tls \
        --allow-loopback-peers --log-file=stdout "$@")
    CONTAINERS+=("$id")
    for _ in $(seq 100); do
        ss -lun 2>/dev/null | grep -q "127.0.0.1:$port " && return 0
        sleep 0.05
    done
    echo "coturn did not start on $port"; docker logs "$id" 2>&1 | tail -5
    return 1
}

# 例を起動し、LISTENING が出るまで待つ（ログ名, コマンド...）。
start_bin() {
    local log=$1
    shift
    "$@" >"$WORK/$log.out" 2>"$WORK/$log.err" &
    PIDS+=($!)
    for _ in $(seq 100); do
        grep -q LISTENING "$WORK/$log.out" 2>/dev/null && return 0
        sleep 0.05
    done
    echo "$log did not start"; cat "$WORK/$log.err"
    return 1
}

uclient() {
    timeout 60 docker run --rm --network host --entrypoint turnutils_uclient "$IMAGE" "$@" 2>&1
}

# REST 方式の一時資格情報（期限 1 時間後）。パスワードは openssl で独立に計算する。
rest_user() { echo "$(( $(date +%s) + 3600 )):$1"; }
rest_pass() { printf '%s' "$1" | openssl dgst -sha1 -hmac "$2" -binary | base64; }

echo "== ports $BASE..$((BASE + 99))"

# ---------------------------------------------------------------- coturn（長期資格情報）
start_coturn "$COTURN_PORT" $((BASE + 50)) $((BASE + 69)) --lt-cred-mech --user=u:p

# A. senca の STUN クライアント → coturn
if out=$("$STUN_CLIENT" "127.0.0.1:$COTURN_PORT" 2>&1) && grep -q '^MAPPED 127.0.0.1:' <<<"$out"; then
    pass "A stun_client -> coturn ($(head -1 <<<"$out"))"
else
    fail "A stun_client -> coturn: $out"
fi

# B. turnutils_stunclient → senca の STUN 応答器
start_bin stun "$STUN_SERVER" "127.0.0.1:$SENCA_STUN_PORT"
out=$(timeout 30 docker run --rm --network host --entrypoint turnutils_stunclient "$IMAGE" -p "$SENCA_STUN_PORT" 127.0.0.1 2>&1 || true)
if grep -q 'UDP reflexive addr: 127.0.0.1:' <<<"$out"; then
    pass "B turnutils_stunclient -> senca stun_server"
else
    fail "B turnutils_stunclient -> senca stun_server: $out"
fi

# C. senca の TURN クライアント → coturn
out=$("$TURN_CHECK" "127.0.0.1:$COTURN_PORT" u p 20 2>&1 || true)
if grep -q '^ALLOCATED' <<<"$out" && grep -q '^SEND 20/20' <<<"$out" && grep -q '^CHANNEL 20/20' <<<"$out" \
    && grep -q '^CLOSED' <<<"$out"; then
    pass "C turn client -> coturn (allocate, permission, send/data, channel, refresh 0)"
else
    fail "C turn client -> coturn: $out"
fi
out=$("$TURN_CHECK" "127.0.0.1:$COTURN_PORT" u wrong 1 2>&1 || true)
if grep -q 'AllocationFailed { code: Some(401)' <<<"$out"; then
    pass "C turn client -> coturn rejects wrong password (401)"
else
    fail "C turn client -> coturn with wrong password: $out"
fi

# C'. coturn の REST 方式
start_coturn "$COTURN_REST_PORT" $((BASE + 70)) $((BASE + 79)) --use-auth-secret --static-auth-secret="$SECRET"
U=$(rest_user alice)
out=$("$TURN_CHECK" "127.0.0.1:$COTURN_REST_PORT" "$U" "$(rest_pass "$U" "$SECRET")" 10 2>&1 || true)
if grep -q '^SEND 10/10' <<<"$out" && grep -q '^CHANNEL 10/10' <<<"$out"; then
    pass "C turn client -> coturn with REST (use-auth-secret) credentials"
else
    fail "C turn client -> coturn REST: $out"
fi

# F. ICE: coturn の relayed 候補だけで繋がる
out=$(timeout 40 "$ICE_PAIR" --stun "127.0.0.1:$COTURN_PORT" --turn "127.0.0.1:$COTURN_PORT" u p --relay-only 2>&1 || true)
if grep -q '^DATA ok' <<<"$out" && grep -q 'Relayed' <<<"$out"; then
    pass "F ice_pair via coturn relayed candidates"
else
    fail "F ice_pair via coturn relay: $out"
fi

# ---------------------------------------------------------------- senca の TURN サーバー
start_bin turn "$TURN_SERVER" --listen "127.0.0.1:$SENCA_TURN_PORT" --relay-ip 127.0.0.1 \
    --ports $((BASE + 80))-$((BASE + 99)) --user u:p --realm r --allow-peer 127.0.0.0/8

# D. turnutils_uclient → senca の TURN サーバー（peer は coturn の turnutils_peer）
PEER_ID=$(docker run --rm -d --network host --entrypoint turnutils_peer "$IMAGE" -p "$PEER_PORT" -L 127.0.0.1)
CONTAINERS+=("$PEER_ID")
sleep 0.5
check_uclient() { # 名前, 期待する受信数, uclient の引数...
    local name=$1 want=$2
    shift 2
    local out
    out=$(uclient "$@")
    if grep -q "tot_recv_msgs=$want\$" <<<"$out" && grep -q 'Total lost packets 0 ' <<<"$out"; then
        pass "D uclient -> senca turn_server ($name)"
    else
        fail "D uclient -> senca turn_server ($name): $(tail -5 <<<"$out")"
    fi
}
check_uclient channel 40 -c -u u -w p -p "$SENCA_TURN_PORT" -e 127.0.0.1 -r "$PEER_PORT" -n 20 -m 2 127.0.0.1
check_uclient send-indication 40 -c -s -u u -w p -p "$SENCA_TURN_PORT" -e 127.0.0.1 -r "$PEER_PORT" -n 20 -m 2 127.0.0.1
check_uclient relay-to-relay 40 -c -y -u u -w p -p "$SENCA_TURN_PORT" -n 20 -m 2 127.0.0.1
out=$(uclient -c -u u -w wrong -p "$SENCA_TURN_PORT" -e 127.0.0.1 -r "$PEER_PORT" -n 2 -m 1 127.0.0.1 || true)
if grep -q 'Cannot complete Allocation' <<<"$out"; then
    pass "D uclient -> senca turn_server rejects wrong password"
else
    fail "D uclient wrong password: $(tail -5 <<<"$out")"
fi

# D'. REST 方式（coturn の uclient が -W で一時資格情報を作る）
start_bin turn-rest "$TURN_SERVER" --listen "127.0.0.1:$SENCA_TURN_REST_PORT" --relay-ip 127.0.0.1 \
    --ports $((BASE + 40))-$((BASE + 49)) --secret "$SECRET" --realm r --allow-peer 127.0.0.0/8
check_uclient rest 40 -c -W "$SECRET" -u bob -p "$SENCA_TURN_REST_PORT" -e 127.0.0.1 -r "$PEER_PORT" -n 20 -m 2 127.0.0.1
out=$(uclient -c -W wrong-secret -u bob -p "$SENCA_TURN_REST_PORT" -e 127.0.0.1 -r "$PEER_PORT" -n 2 -m 1 127.0.0.1 || true)
if grep -q 'Cannot complete Allocation' <<<"$out"; then
    pass "D uclient -> senca turn_server rejects wrong REST secret"
else
    fail "D uclient wrong REST secret: $(tail -5 <<<"$out")"
fi

# E. senca の TURN クライアント → senca の TURN サーバー
out=$("$TURN_CHECK" "127.0.0.1:$SENCA_TURN_PORT" u p 20 2>&1 || true)
if grep -q '^SEND 20/20' <<<"$out" && grep -q '^CHANNEL 20/20' <<<"$out" && grep -q '^CLOSED' <<<"$out"; then
    pass "E turn client -> senca turn_server"
else
    fail "E turn client -> senca turn_server: $out"
fi

# F. ICE: host どうし、ICE-lite、senca の TURN の relayed 候補だけ
out=$(timeout 30 "$ICE_PAIR" 2>&1 || true)
if grep -q '^DATA ok' <<<"$out"; then pass "F ice_pair host"; else fail "F ice_pair host: $out"; fi
out=$(timeout 30 "$ICE_PAIR" --lite-b 2>&1 || true)
if grep -q '^DATA ok' <<<"$out"; then pass "F ice_pair with ICE-lite peer"; else fail "F ice_pair lite: $out"; fi
out=$(timeout 40 "$ICE_PAIR" --turn "127.0.0.1:$SENCA_TURN_PORT" u p --relay-only 2>&1 || true)
if grep -q '^DATA ok' <<<"$out" && grep -q 'Relayed' <<<"$out"; then
    pass "F ice_pair via senca turn_server relayed candidates"
else
    fail "F ice_pair via senca turn: $out"
fi

summary
