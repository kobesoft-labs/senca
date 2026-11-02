#!/usr/bin/env bash
# senca-paas の相互運用試験（クラウドには接続しない）。
#
#  A. cargo test -p senca-paas（AWS の SigV4 試験集のベクタ、要求の形、応答・誤りの封筒、reconcile、
#     std の TCP で模擬 API を立てた作成 → 取得 → 一覧 → 削除・誤り・途中で落ちたときの後始末）。
#  B. 模擬 API（examples/paas_mock）を curl で叩く。curl の --aws-sigv4 は SigV4 の別の実装なので、
#     senca-paas の検証器（＝同じ正規化を使う署名器）が独立した署名を受け付けることを確かめる。
#     誤った secret・誤った service・誤った Bearer トークンは拒否されること。
#
# 環境変数: CARGO（既定 ~/.cargo/bin/cargo）、CARGO_TARGET_DIR（既定 ~/.cache/senca-target-paas）。
set -euo pipefail

ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/../.." && pwd)"
CARGO="${CARGO:-$HOME/.cargo/bin/cargo}"
export CARGO_TARGET_DIR="${CARGO_TARGET_DIR:-$HOME/.cache/senca-target-paas}"
mkdir -p "$HOME/.cache"
WORK="$(mktemp -d "$HOME/.cache/senca-paas-interop.XXXXXX")"
SERVER_PID=""
cleanup() {
    if [[ -n "$SERVER_PID" ]]; then kill "$SERVER_PID" 2>/dev/null || true; wait "$SERVER_PID" 2>/dev/null || true; fi
    rm -rf "$WORK"
}
trap cleanup EXIT

PASS=0
FAIL=0
pass() { echo "PASS: $1"; PASS=$((PASS + 1)); }
fail() { echo "FAIL: $1"; FAIL=$((FAIL + 1)); }
check() { # check <名前> <期待> <実際>
    if [[ "$2" == "$3" ]]; then pass "$1"; else fail "$1 (expected '$2', got '$3')"; fi
}

# ---- A ----
if "$CARGO" test -q --manifest-path "$ROOT/Cargo.toml" -p senca-paas >"$WORK/test.log" 2>&1; then
    pass "cargo test -p senca-paas ($(grep -Eo '[0-9]+ passed' "$WORK/test.log" | awk '{s+=$1} END {print s}') tests)"
else
    fail "cargo test -p senca-paas"; tail -40 "$WORK/test.log"
fi

# ---- B ----
if ! curl --help all 2>/dev/null | grep -q -- '--aws-sigv4'; then
    echo "SKIP: curl に --aws-sigv4 が無い（7.75 以降が要る）"
else
    CF_ACCOUNT=0123456789abcdef0123456789abcdef
    CF_TOKEN="cf-interop-$RANDOM$RANDOM"
    AKID=AKIAINTEROP000000001
    SECRET="interop/secret+$RANDOM$RANDOM$RANDOM"
    REGION=ap-northeast-1
    "$CARGO" build -q --manifest-path "$ROOT/Cargo.toml" -p senca-paas --example paas_mock
    MOCK_CF_ACCOUNT=$CF_ACCOUNT MOCK_CF_TOKEN=$CF_TOKEN MOCK_AWS_KEY_ID=$AKID MOCK_AWS_SECRET=$SECRET MOCK_AWS_REGION=$REGION \
        "$CARGO_TARGET_DIR/debug/examples/paas_mock" 0 >"$WORK/addr" 2>"$WORK/server.log" &
    SERVER_PID=$!
    for _ in $(seq 1 50); do grep -q listening "$WORK/addr" 2>/dev/null && break; sleep 0.1; done
    BASE="http://$(awk '{print $2}' "$WORK/addr")"

    code() { curl -s -D "$WORK/headers" -o "$WORK/body" -w '%{http_code}' "$@"; }
    jget() { python3 -c "import json,sys;d=json.load(open(sys.argv[1]));print(eval('d'+sys.argv[2]))" "$WORK/body" "$1"; }
    sig() { echo "aws:amz:$REGION:$1"; }

    # Cloudflare（Bearer）
    check "cloudflare create" 201 "$(code -X POST -H "Authorization: Bearer $CF_TOKEN" -H 'Content-Type: application/json' \
        -d '{"meta":{"senca:owner":"interop"}}' "$BASE/client/v4/accounts/$CF_ACCOUNT/stream/live_inputs")"
    UID_CF=$(jget "['result']['uid']")
    check "cloudflare meta tag" interop "$(jget "['result']['meta']['senca:owner']")"
    check "cloudflare list" 1 "$(code -H "Authorization: Bearer $CF_TOKEN" "$BASE/client/v4/accounts/$CF_ACCOUNT/stream/live_inputs" >/dev/null; jget "['result']['total']")"
    check "cloudflare wrong token" 403 "$(code -H 'Authorization: Bearer nope' "$BASE/client/v4/accounts/$CF_ACCOUNT/stream/live_inputs")"
    check "cloudflare error envelope" "False 10000" "$(jget "['success']") $(jget "['errors'][0]['code']")"
    check "cloudflare delete" 200 "$(code -X DELETE -H "Authorization: Bearer $CF_TOKEN" "$BASE/client/v4/accounts/$CF_ACCOUNT/stream/live_inputs/$UID_CF")"
    check "cloudflare delete again" 404 "$(code -X DELETE -H "Authorization: Bearer $CF_TOKEN" "$BASE/client/v4/accounts/$CF_ACCOUNT/stream/live_inputs/$UID_CF")"

    # IVS（curl の SigV4 で署名）
    check "ivs CreateChannel (curl sigv4)" 200 "$(code --aws-sigv4 "$(sig ivs)" --user "$AKID:$SECRET" -X POST \
        -H 'Content-Type: application/json' -d '{"name":"interop","tags":{"senca:owner":"interop"}}' "$BASE/CreateChannel")"
    ARN=$(jget "['channel']['arn']")
    check "ivs stream key returned" yes "$(jget "['streamKey']['value']" | grep -q '^sk_' && echo yes || echo no)"
    check "ivs ListChannels" 1 "$(code --aws-sigv4 "$(sig ivs)" --user "$AKID:$SECRET" -X POST -H 'Content-Type: application/json' \
        -d '{"maxResults":100}' "$BASE/ListChannels" >/dev/null; jget "['channels'].__len__()")"
    check "ivs wrong secret" 403 "$(code --aws-sigv4 "$(sig ivs)" --user "$AKID:wrong" -X POST -H 'Content-Type: application/json' -d '{}' "$BASE/ListChannels")"
    check "ivs wrong secret error type" yes "$(grep -qi '^x-amzn-errortype: InvalidSignatureException' "$WORK/headers" && echo yes || echo no)"
    check "ivs wrong service scope" 403 "$(code --aws-sigv4 "$(sig medialive)" --user "$AKID:$SECRET" -X POST -H 'Content-Type: application/json' -d '{}' "$BASE/ListChannels")"
    check "ivs tampered body" 403 "$(code --aws-sigv4 "$(sig ivs)" --user "$AKID:$SECRET" -X POST -H 'Content-Type: application/json' \
        -H 'X-Amz-Content-Sha256: 0000000000000000000000000000000000000000000000000000000000000000' -d '{}' "$BASE/ListChannels")"
    check "ivs DeleteChannel" 204 "$(code --aws-sigv4 "$(sig ivs)" --user "$AKID:$SECRET" -X POST -H 'Content-Type: application/json' \
        -d "{\"arn\":\"$ARN\"}" "$BASE/DeleteChannel")"

    # MediaLive（GET の query の正規化・DELETE）
    check "medialive CreateInputSecurityGroup" 200 "$(code --aws-sigv4 "$(sig medialive)" --user "$AKID:$SECRET" -X POST \
        -H 'Content-Type: application/json' -d '{"whitelistRules":[{"cidr":"203.0.113.1/32"}],"tags":{"senca:owner":"interop"}}' "$BASE/prod/inputSecurityGroups")"
    SG=$(jget "['securityGroup']['id']")
    check "medialive CreateInput" 201 "$(code --aws-sigv4 "$(sig medialive)" --user "$AKID:$SECRET" -X POST -H 'Content-Type: application/json' \
        -d "{\"name\":\"interop\",\"type\":\"RTMP_PUSH\",\"inputSecurityGroups\":[\"$SG\"],\"destinations\":[{\"streamName\":\"senca/x\"}],\"tags\":{\"senca:owner\":\"interop\"}}" \
        "$BASE/prod/inputs")"
    INPUT=$(jget "['input']['id']")
    check "medialive ListInputs (query, curl sorts)" 1 "$(code --aws-sigv4 "$(sig medialive)" --user "$AKID:$SECRET" \
        "$BASE/prod/inputs?nextToken=0&maxResults=10" >/dev/null; jget "['inputs'].__len__()")"
    check "medialive DescribeInput" "$INPUT" "$(code --aws-sigv4 "$(sig medialive)" --user "$AKID:$SECRET" "$BASE/prod/inputs/$INPUT" >/dev/null; jget "['id']")"
    check "medialive DeleteInputSecurityGroup in use" 409 "$(code --aws-sigv4 "$(sig medialive)" --user "$AKID:$SECRET" -X DELETE "$BASE/prod/inputSecurityGroups/$SG")"
    check "medialive DeleteInput" 200 "$(code --aws-sigv4 "$(sig medialive)" --user "$AKID:$SECRET" -X DELETE "$BASE/prod/inputs/$INPUT")"
    check "medialive DeleteInputSecurityGroup" 200 "$(code --aws-sigv4 "$(sig medialive)" --user "$AKID:$SECRET" -X DELETE "$BASE/prod/inputSecurityGroups/$SG")"
    check "medialive unsigned" 403 "$(code "$BASE/prod/inputs")"

    # 秘密がサーバーのログに出ていない
    if grep -qF -e "$SECRET" -e "$CF_TOKEN" "$WORK/server.log"; then fail "secrets in server log"; else pass "no secrets in server log"; fi
fi

echo "== $PASS passed, $FAIL failed"
[[ "$FAIL" == 0 ]]
