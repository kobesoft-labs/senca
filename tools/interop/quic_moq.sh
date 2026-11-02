#!/usr/bin/env bash
# QUIC・HTTP/3・WebTransport・MoQ（draft-22）の相互接続試験（AC-09・AC-10）。
#
#   A. QUIC v1: aioquic（独立実装）の client・server と senca-quic の echo
#      （通常・損失・鍵更新・移行・Retry）、OpenSSL 3.5 の `s_client -quic` のハンドシェイク
#   B. HTTP/3 と WebTransport: Docker の Chrome（QUICHE）で GET・POST・WebTransport の
#      双方向・単方向 stream・datagram・session の close
#   C. MoQ draft-22: moq-dev/moq（`moq`・`moq-relay`、Rust・quinn・web-transport-quinn）と
#      senca-moq の relay・publisher・subscriber を native QUIC と WebTransport で相互に
#   D. MoQ over WebTransport: Chrome の WebTransport の上で最小の MoQ subscriber（JS）
#
# 使い方: tools/interop/quic_moq.sh
#   KEEP=1                     作業場所を残す
#   NO_BROWSER=1               B・D を飛ばす
#   NO_MOQ_DEV=1               C を飛ばす
#   MOQ_BIN_DIR=dir            moq・moq-relay の場所（無ければ MOQ_BUILD=1 で
#                              github.com/moq-dev/moq を $HOME/.cache に取ってきて作る）
#   AIOQUIC_PY=python          aioquic の入った Python（無ければ uv で $HOME/.cache に作る）
#   BROWSER_IMAGE=image        Chrome と playwright-core の入ったイメージ（既定 ov-chrome:latest）
set -euo pipefail
ROOT=$(cd "$(dirname "$0")/../.." && pwd)
HERE="$ROOT/tools/interop/quic_moq"
CACHE="$HOME/.cache"
TARGET=${CARGO_TARGET_DIR:-$CACHE/senca-target-quic}
CARGO=${CARGO:-$HOME/.cargo/bin/cargo}
mkdir -p "$CACHE"
W=$(mktemp -d "$CACHE/senca-quic-moq-interop.XXXXXX")
PIDS=()
CONTAINERS=()
PASS=0; FAIL=0; SKIP=0
pass() { echo "PASS $*"; PASS=$((PASS+1)); }
fail() { echo "FAIL $*"; FAIL=$((FAIL+1)); }
skip() { echo "SKIP $*"; SKIP=$((SKIP+1)); }
cleanup() {
  for c in "${CONTAINERS[@]}"; do docker stop "$c" >/dev/null 2>&1 || true; done
  for p in "${PIDS[@]}"; do kill "$p" 2>/dev/null || true; done
  wait 2>/dev/null || true
  if [ -n "${KEEP:-}" ]; then echo "kept $W"; else rm -rf "$W"; fi
}
trap cleanup EXIT INT TERM

# 51000〜51999 の空いている UDP ポート
port() { python3 -c '
import random,socket
for _ in range(500):
    p=random.randint(51000,51999); s=socket.socket(socket.AF_INET,socket.SOCK_DGRAM)
    try: s.bind(("127.0.0.1",p)); s.close(); print(p); break
    except OSError: pass'; }
bg() { "$@" & PIDS+=($!); }
# 標準入力をファイルにして背景で動かす（背景のコマンドは明示しないと /dev/null を読む）
bg_in() { local f=$1; shift; "$@" < "$f" & PIDS+=($!); }
stop() { kill "$1" 2>/dev/null || true; wait "$1" 2>/dev/null || true; }
jq_get() { python3 -c "import json,sys; d=json.loads(sys.stdin.read().strip().splitlines()[-1]); print(d.get('$1'))"; }

echo "== build"
(cd "$ROOT" && CARGO_TARGET_DIR=$TARGET "$CARGO" build -q -p senca-quic -p senca-h3 -p senca-moq --examples)
EX=$TARGET/debug/examples
QUIC_ECHO=$EX/quic_echo; H3_SERVER=$EX/h3_server; MOQ=$EX/moq_interop

# 証明書: 試験用 CA の証明書（localhost・127.0.0.1）と、ブラウザー用の自己署名（ECDSA P-256、14 日以内）
DATA="$ROOT/crates/senca-tls/tests/data"
CA=$DATA/ca.pem; CERT=$DATA/server-ecdsa.pem; KEY=$DATA/server-ecdsa.key
openssl req -x509 -newkey ec -pkeyopt ec_paramgen_curve:prime256v1 -nodes -keyout "$W/bkey.pem" -out "$W/bcert.pem" \
  -days 10 -subj /CN=localhost -addext "subjectAltName=DNS:localhost,IP:127.0.0.1" 2>/dev/null
B_SPKI=$(openssl x509 -in "$W/bcert.pem" -pubkey -noout | openssl pkey -pubin -outform der | openssl dgst -sha256 -binary | base64)
B_SPKI_HEX=$(openssl x509 -in "$W/bcert.pem" -pubkey -noout | openssl pkey -pubin -outform der | openssl dgst -sha256 -hex | awk '{print $2}')
B_CERT_HASH=$(openssl x509 -in "$W/bcert.pem" -outform der | openssl dgst -sha256 -hex | awk '{print $2}')

# ------------------------------------------------------------------ A. QUIC
echo "== A. QUIC v1 (aioquic, OpenSSL)"
PY=${AIOQUIC_PY:-}
if [ -z "$PY" ]; then
  if [ -x "$CACHE/senca-quic-work/venv/bin/python" ] && "$CACHE/senca-quic-work/venv/bin/python" -c 'import aioquic' 2>/dev/null; then
    PY=$CACHE/senca-quic-work/venv/bin/python
  elif command -v uv >/dev/null || [ -x "$HOME/.local/bin/uv" ]; then
    UV=$(command -v uv || echo "$HOME/.local/bin/uv")
    "$UV" venv -q "$CACHE/senca-quic-work/venv" 2>/dev/null || true
    "$UV" pip install -q --python "$CACHE/senca-quic-work/venv/bin/python" aioquic >/dev/null 2>&1 || true
    "$CACHE/senca-quic-work/venv/bin/python" -c 'import aioquic' 2>/dev/null && PY=$CACHE/senca-quic-work/venv/bin/python
  fi
fi
if [ -n "$PY" ]; then
  P=$(port)
  bg "$QUIC_ECHO" server --listen 127.0.0.1:$P --cert "$CERT" --key "$KEY" > "$W/echo.log" 2>&1; ES=$!
  sleep 0.3
  for opt in "" "--loss 0.05" "--key-update" "--migrate" "--loss 0.03 --key-update --migrate"; do
    # shellcheck disable=SC2086
    out=$(timeout 70 "$PY" -I "$HERE/quic_peer.py" client 127.0.0.1 $P --ca "$CA" --bytes 2000000 $opt 2>"$W/aio.err" || true)
    if [ "$(echo "$out" | jq_get match)" = True ]; then pass "A aioquic client -> senca server 2MB echo ${opt:-(plain)} $out"; else fail "A aioquic client -> senca server ${opt:-(plain)}: $out $(tail -2 "$W/aio.err")"; fi
  done
  grep -q "key update" "$W/echo.log" && pass "A senca server observed peer-initiated key update" || fail "A senca server did not see key update"
  grep -q "migrated to" "$W/echo.log" && pass "A senca server validated client migration (PATH_CHALLENGE/new CID)" || fail "A no migration seen"
  stop $ES
  # Retry（アドレス検証）
  P=$(port)
  bg "$QUIC_ECHO" server --listen 127.0.0.1:$P --cert "$CERT" --key "$KEY" --retry > "$W/echo-retry.log" 2>&1; ES=$!
  sleep 0.3
  out=$(timeout 30 "$PY" -I "$HERE/quic_peer.py" client 127.0.0.1 $P --ca "$CA" --bytes 200000 2>/dev/null || true)
  [ "$(echo "$out" | jq_get match)" = True ] && pass "A aioquic through senca Retry (token validation)" || fail "A Retry: $out"
  stop $ES
  # senca client -> aioquic server
  P=$(port)
  bg "$PY" -I "$HERE/quic_peer.py" server 127.0.0.1 $P --cert "$CERT" --key "$KEY" > "$W/aio-server.log" 2>&1; AS=$!
  sleep 1
  for opt in "" "--key-update" "--migrate"; do
    # shellcheck disable=SC2086
    if timeout 40 "$QUIC_ECHO" client --connect 127.0.0.1:$P --ca "$CA" --bytes 2000000 $opt > "$W/cli.log" 2>&1; then
      pass "A senca client -> aioquic server 2MB echo ${opt:-(plain)} ($(grep -o 'in [0-9.]*m\?s' "$W/cli.log" | head -1))"
    else fail "A senca client -> aioquic server ${opt:-(plain)}: $(tail -2 "$W/cli.log")"; fi
  done
  stop $AS
else
  skip "A aioquic not available (set AIOQUIC_PY or install uv)"
fi
# OpenSSL 3.5 の QUIC client
if openssl s_client -help 2>&1 | grep -q -- '-quic'; then
  P=$(port)
  bg "$QUIC_ECHO" server --listen 127.0.0.1:$P --cert "$CERT" --key "$KEY" --alpn h3,senca-echo > "$W/echo-ossl.log" 2>&1; ES=$!
  sleep 0.3
  o=$(echo | timeout 10 openssl s_client -quic -alpn h3 -connect 127.0.0.1:$P -servername localhost -CAfile "$CA" 2>&1 || true)
  if echo "$o" | grep -q "Protocol: QUICv1" && echo "$o" | grep -q "Verification: OK"; then pass "A openssl s_client -quic handshake (QUICv1, ALPN h3, verified)"; else fail "A openssl s_client -quic: $(echo "$o" | tail -3)"; fi
  stop $ES
else
  skip "A openssl without -quic"
fi

# ------------------------------------------------------------------ B. Chrome: HTTP/3・WebTransport
IMG=
if [ -z "${NO_BROWSER:-}" ] && command -v docker >/dev/null && docker info >/dev/null 2>&1; then
  IMG=${BROWSER_IMAGE:-ov-chrome:latest}
  docker image inspect "$IMG" >/dev/null 2>&1 || IMG=
fi
browser_checks() { # $1 = result.json, $2 = prefix
  if [ -s "$1" ]; then
    while IFS=$'\t' read -r ok name detail; do
      if [ "$ok" = True ]; then pass "$2 $name"; else fail "$2 $name — $detail"; fi
    done < <(python3 -c 'import json,sys
for c in json.load(open(sys.argv[1]))["checks"]: print("%s\t%s\t%s" % (c["ok"], c["name"], c["detail"].replace("\n"," ").replace("\t"," ")[:300]))' "$1")
  else fail "$2 browser produced no result"; fi
}
echo "== B. HTTP/3 + WebTransport (Chrome)"
if [ -n "$IMG" ]; then
  P=$(port)
  bg "$H3_SERVER" --listen 127.0.0.1:$P --cert "$W/bcert.pem" --key "$W/bkey.pem" > "$W/h3.log" 2>&1; HS=$!
  sleep 0.3
  cp "$HERE/browser_h3wt.cjs" "$W/"
  CONTAINERS+=(senca-quic-moq-b-$$)
  timeout 120 docker run --rm --name senca-quic-moq-b-$$ --network host --ipc=host -v "$W:$W" "$IMG" bash -c \
    "node '$W/browser_h3wt.cjs' $P '$B_SPKI' $B_CERT_HASH '$W/h3wt.json'; chown -R $(id -u):$(id -g) '$W'" > "$W/browser-b.log" 2>&1 || true
  browser_checks "$W/h3wt.json" "B"
  grep -q "closed code=7 reason=\"bye\"" "$W/h3.log" && pass "B server received CLOSE_WEBTRANSPORT_SESSION(7, bye)" || fail "B no close capsule seen"
  stop $HS
else
  skip "B no Docker/Chrome image"
fi

# ------------------------------------------------------------------ C. MoQ draft-22 と moq-dev/moq
echo "== C. MoQ draft-22 (moq-dev/moq)"
MOQ_BIN_DIR=${MOQ_BIN_DIR:-$CACHE/senca-quic-work/moq-target/debug}
if [ -z "${NO_MOQ_DEV:-}" ] && { [ ! -x "$MOQ_BIN_DIR/moq" ] || [ ! -x "$MOQ_BIN_DIR/moq-relay" ]; } && [ -n "${MOQ_BUILD:-}" ]; then
  SRC=$CACHE/senca-quic-work/moq-src
  [ -d "$SRC/.git" ] || git clone -q --depth 1 https://github.com/moq-dev/moq "$SRC"
  (cd "$SRC" && RUSTUP_TOOLCHAIN=stable CARGO_TARGET_DIR=$CACHE/senca-quic-work/moq-target "$CARGO" build -q -j3 -p moq-cli --no-default-features --features quic,websocket \
     && RUSTUP_TOOLCHAIN=stable CARGO_TARGET_DIR=$CACHE/senca-quic-work/moq-target "$CARGO" build -q -j3 -p moq-relay --bin moq-relay)
  MOQ_BIN_DIR=$CACHE/senca-quic-work/moq-target/debug
fi
if [ -z "${NO_MOQ_DEV:-}" ] && [ -x "$MOQ_BIN_DIR/moq" ] && [ -x "$MOQ_BIN_DIR/moq-relay" ]; then
  M=$MOQ_BIN_DIR/moq; MR=$MOQ_BIN_DIR/moq-relay
  echo "moq-dev: $("$M" --version 2>/dev/null | head -1), $("$MR" --version 2>/dev/null | head -1)"
  FF=${FFMPEG:-$HOME/tools/ffmpeg}
  "$FF" -v error -y -f lavfi -i testsrc=size=640x360:rate=30 -t 6 -c:v libx264 -preset ultrafast -g 30 -keyint_min 30 \
    -pix_fmt yuv420p -movflags +frag_keyframe+empty_moov+default_base_moof -f mp4 "$W/test.mp4"
  MCON=(--connect-version moq-transport-22 --connect-tls-root "$CA")

  # C1/C2: moq-dev の publisher（native QUIC）→ senca relay → senca subscriber・moq-dev の fetch
  P=$(port)
  bg "$MOQ" relay --listen 127.0.0.1:$P --cert "$CERT" --key "$KEY" > "$W/srelay.log" 2>&1; SR=$!
  sleep 0.3
  bg_in "$W/test.mp4" timeout 60 "$M" --connect moqt://localhost:$P/ "${MCON[@]}" --broadcast live/demo import fmp4 > "$W/mimport.log" 2>&1; MI=$!
  sleep 2
  o=$(timeout 10 "$MOQ" subscribe --connect 127.0.0.1:$P --ca "$CA" --namespace live/demo --track catalog.json --count 1 --seconds 6 --out "$W/cat.json" || true)
  if python3 -c 'import json,sys; c=json.load(open(sys.argv[1])); assert "video" in c' "$W/cat.json" 2>/dev/null; then
    pass "C moq import (native) -> senca relay -> senca subscriber: hang catalog.json ($(head -c 60 "$W/cat.json")...)"
  else fail "C catalog via senca relay: $o"; fi
  VT=$(python3 -c 'import json,sys; print(next(iter(json.load(open(sys.argv[1]))["video"]["renditions"])))' "$W/cat.json" 2>/dev/null || echo 0.mp4-v)
  o=$(timeout 10 "$MOQ" subscribe --connect 127.0.0.1:$P --ca "$CA" --namespace live/demo --track "$VT" --seconds 5 || true)
  [ "$(echo "$o" | jq_get ok)" = True ] && [ "$(echo "$o" | jq_get objects)" -gt 0 ] && pass "C moq import -> senca relay -> senca subscriber: video track '$VT' $o" || fail "C video via senca relay: $o"
  for g in 1 3 5; do
    n=$(timeout 10 "$M" --connect moqt://localhost:$P/ "${MCON[@]}" --broadcast live/demo fetch --json --group $g "$VT" 2>"$W/mf$g.log" | wc -l || true)
    [ "$n" -gt 0 ] && pass "C moq fetch (native) group $g via senca relay: $n frames (TRACK_STATUS/FETCH passthrough + cache)" || fail "C moq fetch group $g: $(tail -1 "$W/mf$g.log")"
  done
  n=$(timeout 10 "$M" --connect moqt://localhost:$P/ "${MCON[@]}" --broadcast live/demo fetch --json "$VT" 2>"$W/mfl.log" | wc -l || true)
  [ "$n" -gt 0 ] && pass "C moq fetch latest group via senca relay: $n frames (SUBSCRIBE+FILL)" || fail "C moq fetch latest: $(tail -1 "$W/mfl.log")"
  # 版の不一致: moq-transport-20 だけを出す client は拒否される（ALPN moqt-20、推測で接続しない）
  if timeout 10 "$M" --connect moqt://localhost:$P/ --connect-version moq-transport-20 --connect-tls-root "$CA" --connect-timeout 3s --broadcast live/demo fetch catalog.json > /dev/null 2> "$W/mold.log"; then
    fail "C moq-transport-20 client (moq-dev) was accepted"
  else pass "C unknown version moqt-20 from moq-dev refused by senca relay"; fi
  if [ -n "$PY" ]; then
    o=$(timeout 10 "$PY" -I "$HERE/quic_peer.py" client 127.0.0.1 $P --ca "$CA" --alpn moqt-20 --bytes 10 2>/dev/null || true)
    echo "$o" | grep -q '"terminated": \[376' && pass "C unknown version (ALPN moqt-20) closed with CRYPTO_ERROR 0x178 no_application_protocol (aioquic): $o" \
      || fail "C ALPN moqt-20 close code: $o"
  fi
  stop $MI

  # C3: moq-dev の publisher・fetch（WebTransport）→ senca relay
  bg_in "$W/test.mp4" timeout 60 "$M" --connect https://localhost:$P/moq "${MCON[@]}" --broadcast live/wt import fmp4 > "$W/mimport-wt.log" 2>&1; MI=$!
  sleep 3
  grep -q "connected.*webtransport" "$W/mimport-wt.log" && pass "C moq import connected to senca relay over WebTransport (moqt-22)" || fail "C moq WebTransport connect: $(tail -2 "$W/mimport-wt.log")"
  o=$(timeout 10 "$MOQ" subscribe --connect 127.0.0.1:$P --ca "$CA" --namespace live/wt --track "$VT" --seconds 5 || true)
  [ "$(echo "$o" | jq_get ok)" = True ] && [ "$(echo "$o" | jq_get objects)" -gt 0 ] && pass "C moq import (WebTransport) -> senca relay -> senca subscriber (native): $o" || fail "C WT publisher via senca relay: $o"
  bad=0; tot=0
  for i in 1 2 3 4 5 6; do
    n=$(timeout 10 "$M" --connect https://localhost:$P/moq "${MCON[@]}" --broadcast live/wt fetch --json --group $((i % 6)) "$VT" 2>"$W/mwf.log" | wc -l || true)
    tot=$((tot+1)); { [ "$n" -gt 0 ] && ! grep -q violation "$W/mwf.log"; } || bad=$((bad+1))
  done
  [ $bad = 0 ] && pass "C moq fetch over WebTransport via senca relay: $tot/$tot ok" || fail "C moq fetch over WebTransport: $bad/$tot failed"
  stop $MI; stop $SR

  # C4: senca publisher → moq-relay → senca subscriber（native・WebTransport）・moq-dev の fetch
  P=$(port)
  bg "$MR" --listen 127.0.0.1:$P --listen-tls-cert "$CERT" --listen-tls-key "$KEY" --auth-public '**' > "$W/mrelay.log" 2>&1; MRP=$!
  sleep 1
  bg "$MOQ" publish --connect 127.0.0.1:$P --ca "$CA" --namespace live/senca --fmp4 "$W/test.mp4" --seconds 40 > "$W/spub.out" 2> "$W/spub.log"; SP=$!
  sleep 2
  o=$(timeout 10 "$MOQ" subscribe --connect 127.0.0.1:$P --ca "$CA" --namespace live/senca --track video --count 6 --seconds 8 --out "$W/v1.bin" || true)
  [ "$(echo "$o" | jq_get objects)" = 6 ] && pass "C senca publisher -> moq-relay -> senca subscriber (native): $o" || fail "C via moq-relay native: $o"
  o=$(timeout 10 "$MOQ" subscribe --wt --path / --connect 127.0.0.1:$P --ca "$CA" --namespace live/senca --track video --count 6 --seconds 8 --out "$W/v2.bin" || true)
  [ "$(echo "$o" | jq_get objects)" = 6 ] && pass "C senca WebTransport client -> moq-relay (web-transport-quinn): $o" || fail "C senca WT client to moq-relay: $o"
  cmp -s "$W/v1.bin" "$W/v2.bin" && pass "C native and WebTransport subscribers got byte-identical objects" || fail "C native/WT payload mismatch"
  c=$(timeout 10 "$M" --connect moqt://localhost:$P/ "${MCON[@]}" --broadcast live/senca fetch catalog.json 2>/dev/null || true)
  echo "$c" | grep -q '"kind":"cmaf"' && pass "C moq fetch of senca catalog (hang, CMAF) via moq-relay" || fail "C moq fetch senca catalog: $(echo "$c" | head -c 100)"
  n=$(timeout 10 "$M" --connect moqt://localhost:$P/ "${MCON[@]}" --broadcast live/senca fetch --group 2 video 2>"$W/msv.log" | wc -c || true)
  [ "$n" -gt 0 ] && pass "C moq fetch of senca video group 2 via moq-relay: $n bytes" || fail "C moq fetch senca video: $(tail -1 "$W/msv.log")"
  grep -q "closed" "$W/spub.log" && fail "C senca publisher session closed: $(grep closed "$W/spub.log")" || pass "C senca publisher handled moq-relay requests (REQUEST_UPDATE, FETCH) without error"
  stop $SP; stop $MRP
else
  skip "C moq-dev binaries not found (MOQ_BIN_DIR, or MOQ_BUILD=1 to build)"
fi

# ------------------------------------------------------------------ D. Chrome: MoQ over WebTransport
echo "== D. MoQ over WebTransport (Chrome)"
if [ -n "$IMG" ]; then
  P=$(port); PP=$(port)
  bg "$H3_SERVER" --listen 127.0.0.1:$PP --cert "$W/bcert.pem" --key "$W/bkey.pem" > "$W/h3page.log" 2>&1; HS=$!
  bg "$MOQ" relay --listen 127.0.0.1:$P --cert "$W/bcert.pem" --key "$W/bkey.pem" > "$W/drelay.log" 2>&1; DR=$!
  sleep 0.3
  bg "$MOQ" publish --connect 127.0.0.1:$P --pin-spki "$B_SPKI_HEX" --namespace live/demo --objects 30 --seconds 60 > "$W/dpub.out" 2> "$W/dpub.log"; DP=$!
  sleep 1
  cp "$HERE/browser_moq.cjs" "$W/"
  CONTAINERS+=(senca-quic-moq-d-$$)
  timeout 120 docker run --rm --name senca-quic-moq-d-$$ --network host --ipc=host -v "$W:$W" "$IMG" bash -c \
    "node '$W/browser_moq.cjs' $P '$B_SPKI' $B_CERT_HASH '$W/moqwt.json' live/demo video 30 $PP; chown -R $(id -u):$(id -g) '$W'" > "$W/browser-d.log" 2>&1 || true
  browser_checks "$W/moqwt.json" "D"
  stop $DP; stop $DR; stop $HS
else
  skip "D no Docker/Chrome image"
fi

echo "== $PASS passed, $FAIL failed, $SKIP skipped"
[ "$FAIL" = 0 ]
