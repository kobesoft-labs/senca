#!/usr/bin/env bash
# 管理 API の文書（docs/api/openapi.yaml）と実装の突き合わせ。
#  (a) 静的: api/mod.rs の経路表と openapi.yaml の (method, path) の過不足、x-senca-scope と handler の権限
#  (b) 実地: ノードを起動し、すべての操作に実際の要求を送って、状態コードが文書にあり本文がスキーマに合うか
# 使い方: tools/interop/openapi.sh（KEEP=1 で作業場所を残す、NODE_BIN_PREBUILT=1 でビルドしない）
source "$(dirname "$0")/lib.sh"
SPEC=$ROOT/docs/api/openapi.yaml
CHECK=$ROOT/tools/interop/schema_check.py
CARGO=${CARGO:-$(command -v cargo || echo "$HOME/.cargo/bin/cargo")}

echo "== (a) 静的検査"
python3 "$CHECK" routes "$SPEC" "$ROOT/crates/senca-node/src/api" | tee /dev/stderr | grep -q '^== .* 0 failed$' \
  && pass "static: 経路表と openapi.yaml" || fail "static: 経路表と openapi.yaml"

[ -n "${NODE_BIN_PREBUILT:-}" ] || (cd "$ROOT" && "$CARGO" build -q -p senca-node)
[ -x "$NODE_BIN" ] || { echo "node binary not found: $NODE_BIN"; exit 1; }

W=$(mktemp -d); NODE_PID=
cleanup() { [ -n "$NODE_PID" ] && kill "$NODE_PID" 2>/dev/null || true; wait 2>/dev/null || true; [ -n "${KEEP:-}" ] && echo "kept $W" || rm -rf "$W"; }
trap cleanup EXIT
HTTP=$(port); RTMP=$(port)
"$NODE_BIN" --data-dir "$W/data" --http 127.0.0.1:$HTTP --rtmp 127.0.0.1:$RTMP \
  --public-rtmp-url rtmp://127.0.0.1:$RTMP --public-http-url http://127.0.0.1:$HTTP --reconnect-grace 1000 >>"$W/node.log" 2>&1 &
NODE_PID=$!
for _ in $(seq 100); do curl -sf http://127.0.0.1:$HTTP/readyz >/dev/null && break; sleep 0.1; done
curl -sf http://127.0.0.1:$HTTP/readyz >/dev/null || { echo "node did not start"; cat "$W/node.log"; exit 1; }
TOKEN=$(cat "$W/data/control/bootstrap-admin-token")

# 6 秒の素材（publish 中に API を叩く）
"$FFMPEG" -v error -f lavfi -i testsrc2=size=320x180:rate=30 -f lavfi -i sine=frequency=440:sample_rate=48000 -t 6 \
  -c:v libx264 -g 30 -pix_fmt yuv420p -c:a aac -b:a 64k -f flv "$W/src.flv"

echo "== (b) 実地検査"
if python3 "$CHECK" live "$SPEC" "http://127.0.0.1:$HTTP" "$TOKEN" "rtmp://127.0.0.1:$RTMP" "$FFMPEG" "$W/src.flv" > "$W/live.out" 2>&1; then
  r=0; else r=$?; fi
cat "$W/live.out"
[ $r = 0 ] && pass "live: すべての操作が文書どおり" || fail "live: 文書と違う応答がある（上の FAIL 行）"
grep -q "sk_" "$W/node.log" && fail "stream key がログに出た" || pass "stream key はログに出ない"
summary
