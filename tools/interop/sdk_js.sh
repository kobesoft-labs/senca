#!/usr/bin/env bash
# @senca/sdk（apps/sdk-js）の試験。
#
#   A. 単体試験（node --test、偽の WebSocket・RTCPeerConnection と fetch）と構文の検査。tsc があれば型の検査（TSC=...）
#   B. 管理 API の client を本物の senca-node に対して（stream: 作成・一覧・If-Match・412・428・Idempotency の再送・SSE、
#      部屋: 作成・一覧・If-Match・参加トークン・参加者・統計・削除）。
#      SDK_NODE_BIN（既定 $CARGO_TARGET_DIR/debug/senca-node）が無ければ作る。作れなければ飛ばす
#   C. 部屋: crates/senca-rooms の rooms_server と、Docker の Chrome（偽のカメラ）で 3 ページが SDK だけで部屋に入る:
#      - それぞれが公開し（alice はサイマルキャスト 3 層）、他の 2 人の映像を復号する（session.stats() の framesDecoded > 0）
#        後から入った人の分は SDK が renegotiate（受信の枠を足す）で受ける
#      - #chat: DataChannel で部屋全体へ ping、宛先つき（シグナリング経由、from 付き）で pong
#      - carol が alice の層を r2 → r0 と選ぶ（RemoteTrack.setLayer、layer.changed で解決）と復号の幅が小さくなる
#      - 順に close() して、残った人に participant.left が届き、サーバーの部屋が空になる
#   D. C と同じことを本物の senca-node で（参加トークンは管理 API の POST /rooms/{id}/tokens、シグナリングは node の /signaling、
#      統計は GET /rooms/{id}/stats）。node が無ければ飛ばす
#
# 使い方: tools/interop/sdk_js.sh（KEEP=1 で作業場所を残す、SKIP_BUILD=1 でビルドしない）
#   BROWSER_IMAGE（既定 ov-chrome:latest）が無ければ C・D を飛ばす。Node.js はホストの node、無ければ NODE_IMAGE の Docker。
set -euo pipefail
source "$(dirname "$0")/lib.sh"
CARGO=${CARGO:-$HOME/.cargo/bin/cargo}
export CARGO_TARGET_DIR=${SDK_TARGET:-$HOME/.cache/senca-target-sdk}
SDK=$ROOT/apps/sdk-js
IMG=${BROWSER_IMAGE:-ov-chrome:latest}
NODE_IMAGE=${NODE_IMAGE:-node:22.23.4-bookworm-slim}
mkdir -p "$HOME/.cache"
W=$(mktemp -d -p "$HOME/.cache" senca-sdk-js.XXXXXX)
PIDS=(); CONTAINERS=()
cleanup() {
  local p c
  for p in "${PIDS[@]}"; do kill "$p" 2>/dev/null || true; done
  for c in "${CONTAINERS[@]}"; do docker stop -t 2 "$c" >/dev/null 2>&1 || true; done
  wait 2>/dev/null || true
  if [ -n "${KEEP:-}" ]; then echo "作業ディレクトリ: $W"; else rm -rf "$W"; fi
}
trap cleanup EXIT
jq_() { python3 -I -c "import json,sys;d=json.load(open(sys.argv[1]));print(eval(sys.argv[2]))" "$1" "$2"; }
wait_http() { local i; for i in $(seq 150); do curl -sf "$1" >/dev/null && return 0; sleep 0.1; done; return 1; }

# Node.js（ホスト、無ければ Docker）
if command -v node >/dev/null 2>&1; then
  runnode() { (cd "$SDK" && node "$@"); }
elif docker image inspect "$NODE_IMAGE" >/dev/null 2>&1; then
  runnode() { docker run --rm --network host -e SENCA_URL -e SENCA_TOKEN -v "$SDK:$SDK:ro" -w "$SDK" "$NODE_IMAGE" node "$@"; }
else
  runnode() { return 127; }
fi

# ------------------------------------------------------------------ A
echo "== A 単体試験"
if runnode --version >/dev/null 2>&1; then
  ok=1
  for f in $(cd "$SDK" && find src test -name '*.js' -o -name '*.mjs' | sort); do
    runnode --check "$f" >/dev/null 2>"$W/check.log" || { ok=0; fail "A syntax $f: $(head -3 "$W/check.log")"; }
  done
  [ $ok = 1 ] && pass "A syntax (node --check)"
  if runnode --test --test-timeout=20000 'test/*.test.mjs' >"$W/unit.log" 2>&1; then
    pass "A unit tests ($(grep -E '^# (tests|pass)' "$W/unit.log" | tr '\n' ' '))"
  else
    fail "A unit tests: $(grep -E '^not ok|# fail' "$W/unit.log" | head -5 | tr '\n' ' ')"
  fi
else
  echo "SKIP A (Node.js が無い)"
fi
TSC=${TSC:-$( { command -v tsc || find "$HOME" -maxdepth 6 -path '*/node_modules/typescript/bin/tsc' 2>/dev/null | head -1; } || true)}
if [ -n "$TSC" ] && runnode --version >/dev/null 2>&1; then
  if (cd "$SDK" && node "$TSC" -p . --noEmit) >"$W/tsc.log" 2>&1; then pass "A type check (tsc $(node "$TSC" --version | awk '{print $2}'), strict, checkJs)"
  else fail "A type check: $(head -5 "$W/tsc.log")"; fi
  # 生成した .d.ts が今のソースと同じ（npm に載せる型が古くない）
  if (cd "$SDK" && node scripts/build.mjs --check "$TSC") >"$W/build.log" 2>&1; then pass "A types/ と API.md は最新"
  else fail "A types/・API.md が古い（node scripts/build.mjs で作り直す）: $(tail -3 "$W/build.log")"; fi
else
  echo "SKIP type check (tsc が無い: TSC=... で指定)"
fi
# 生成した REST の型が openapi.yaml と合う
python3 "$SDK/scripts/gen_types.py" "$ROOT/docs/api/openapi.yaml" "$W" >/dev/null
if cmp -s "$W/openapi.gen.d.ts" "$SDK/src/rest/openapi.gen.d.ts" && cmp -s "$W/operations.gen.js" "$SDK/src/rest/operations.gen.js"; then
  pass "A REST の型・操作の表は docs/api/openapi.yaml と一致"
else
  fail "A src/rest/*.gen.* が openapi.yaml と違う（python3 apps/sdk-js/scripts/gen_types.py で作り直す）"
fi

# ------------------------------------------------------------------ B
echo "== B 管理 API（本物の senca-node）"
# lib.sh の NODE_BIN は source の時点の CARGO_TARGET_DIR で決まるので、ここで決め直す（SDK_NODE_BIN で指定できる）
NODE_BIN=${SDK_NODE_BIN:-$CARGO_TARGET_DIR/debug/senca-node}
if [ ! -x "$NODE_BIN" ] && [ -z "${SKIP_BUILD:-}" ]; then (cd "$ROOT" && "$CARGO" build -q -p senca-node) >"$W/node-build.log" 2>&1 || true; fi
if [ -x "$NODE_BIN" ] && runnode --version >/dev/null 2>&1; then
  NH=$(port); NR=$(port); NU=$(port)
  "$NODE_BIN" --data-dir "$W/data" --http "127.0.0.1:$NH" --rtmp "127.0.0.1:$NR" --webrtc-udp "127.0.0.1:$NU" --public-rtmp-url "rtmp://127.0.0.1:$NR" \
    --public-http-url "http://127.0.0.1:$NH" >"$W/node.log" 2>&1 &
  PIDS+=("$!")
  if wait_http "http://127.0.0.1:$NH/readyz"; then
    export SENCA_URL="http://127.0.0.1:$NH" SENCA_TOKEN; SENCA_TOKEN=$(cat "$W/data/control/bootstrap-admin-token")
    if runnode --test --test-timeout=30000 test/live/node.live.mjs >"$W/live.log" 2>&1; then
      pass "B REST client against senca-node ($(grep -E '^# (tests|pass)' "$W/live.log" | tr '\n' ' '))"
    else
      fail "B REST client against senca-node: $(grep -E '^not ok|Error|expected|actual' "$W/live.log" | head -8 | tr '\n' ' ')"
    fi
  else
    fail "B senca-node did not start: $(tail -3 "$W/node.log")"
  fi
else
  echo "SKIP B (senca-node が無い: $NODE_BIN)"
fi

# ------------------------------------------------------------------ C・D
if ! docker image inspect "$IMG" >/dev/null 2>&1; then echo "SKIP C・D (Docker image $IMG が無い)"; summary; exit; fi
IP=${HOST_IP:-$(hostname -I | awk '{print $1}')}

# 静的ファイル（apps/sdk-js をそのまま ES modules で）と、/token・/status の転送。
#   rooms_server: /token・/status をそのまま転送
#   node: /token → POST /api/v1/rooms/{room}/tokens、/status → GET /api/v1/rooms/{room}/stats（管理 API のトークンで）
cat > "$W/static.py" <<'EOF'
import http.server, json, sys, urllib.error, urllib.parse, urllib.request
PORT, ROOT, UP, MODE, TOKEN = sys.argv[1], sys.argv[2], sys.argv[3], sys.argv[4], (sys.argv[5] if len(sys.argv) > 5 else '')
def call(method, path, body=None):
    req = urllib.request.Request(UP + path, method=method, data=None if body is None else json.dumps(body).encode(),
                                 headers={'Authorization': 'Bearer ' + TOKEN, 'Content-Type': 'application/json'} if TOKEN else {})
    with urllib.request.urlopen(req, timeout=10) as r: return r.read()
class H(http.server.SimpleHTTPRequestHandler):
    extensions_map = {**http.server.SimpleHTTPRequestHandler.extensions_map, '.js': 'text/javascript', '.mjs': 'text/javascript'}
    def __init__(self, *a, **k): super().__init__(*a, directory=ROOT, **k)
    def log_message(self, *a): pass
    def end_headers(self):
        self.send_header('Cache-Control', 'no-store'); super().end_headers()
    def do_GET(self):
        u = urllib.parse.urlparse(self.path); q = dict(urllib.parse.parse_qsl(u.query))
        if u.path in ('/token', '/status'):
            if MODE == 'rooms_server': data = call('GET', self.path)
            elif u.path == '/token':
                # 部屋が無ければ作る（409 = 他のページが先に作った）
                try: call('POST', '/api/v1/rooms', {'id': q['room']})
                except urllib.error.HTTPError as e:
                    if e.code != 409: raise
                t = json.loads(call('POST', '/api/v1/rooms/%s/tokens' % q['room'], {'role': 'sendrecv', 'client_id': q['name'], 'metadata': {'name': q['name']}, 'ttl_secs': 600}))
                data = json.dumps({'token': t['token'], 'signaling_url': t.get('signaling_url')}).encode()
            else:
                try: data = call('GET', '/api/v1/rooms/%s/stats' % q['room'])
                except Exception as e: data = json.dumps({'participants': [], 'error': str(e)}).encode()
            self.send_response(200); self.send_header('Content-Type', 'application/json')
            self.send_header('Content-Length', str(len(data))); self.end_headers(); self.wfile.write(data); return
        return super().do_GET()
http.server.ThreadingHTTPServer(('127.0.0.1', int(PORT)), H).serve_forever()
EOF

cat > "$W/drive.cjs" <<'EOF'
// node drive.cjs <base> <out.json> <timeout_ms> <query>...: 1 つの Chrome で複数のページを開き、結果を集めてから順に閉じる
const fs = require('fs'); const path = require('path'); const { execSync } = require('child_process');
const [base, out, timeout, ...queries] = process.argv.slice(2);
function findPw() {
  try { return require.resolve('playwright-core'); } catch { /* 探す */ }
  const p = execSync("find / -path '*/node_modules/playwright-core/package.json' -not -path '/proc/*' 2>/dev/null | head -1").toString().trim();
  if (!p) throw new Error('playwright-core not found');
  return path.dirname(p);
}
const { chromium } = require(findPw());
const sleep = (ms) => new Promise((r) => setTimeout(r, ms));
(async () => {
  const exe = fs.existsSync('/usr/bin/google-chrome') ? '/usr/bin/google-chrome' : undefined;
  const browser = await chromium.launch({ executablePath: exe, headless: true, args: [
    '--use-fake-device-for-media-stream', '--use-fake-ui-for-media-stream', '--autoplay-policy=no-user-gesture-required'] });
  const ctx = await browser.newContext();
  const pages = []; const logs = {};
  for (const q of queries) {
    const page = await ctx.newPage();
    const name = new URLSearchParams(q).get('name');
    logs[name] = [];
    page.on('console', (m) => logs[name].push(m.type() + ': ' + m.text()));
    page.on('pageerror', (e) => logs[name].push('pageerror: ' + e.message));
    await page.goto(base + '/test/interop/page.html?' + q);
    pages.push([name, page]);
    await sleep(1500);
  }
  const results = {};
  await Promise.all(pages.map(async ([name, page]) => {
    try {
      await page.waitForFunction(() => window.__result !== undefined, null, { timeout: Number(timeout) });
      results[name] = await page.evaluate(() => window.__result);
    } catch (e) { results[name] = { ok: false, error: 'driver: ' + e.message }; }
  }));
  let statusBefore = null;
  try { statusBefore = await (await fetch(base + '/status?room=demo')).json(); } catch (e) { statusBefore = { error: String(e) }; }
  // 順に閉じる。最後の人には先に閉じた人の participant.left が届いている
  const closes = {};
  for (const [name, page] of pages) {
    try { closes[name] = await page.evaluate(() => window.__close()); } catch (e) { closes[name] = { error: e.message }; }
    await sleep(1500);
  }
  await sleep(1500);
  let statusAfter = null;
  try { statusAfter = await (await fetch(base + '/status?room=demo')).json(); } catch (e) { statusAfter = { error: String(e) }; }
  for (const [name] of pages) {
    results[name].consoleErrors = logs[name].filter((l) => (l.startsWith('error') || l.startsWith('pageerror')) && !/status of 404/.test(l));
    results[name].log = logs[name].slice(-25);
  }
  fs.writeFileSync(out, JSON.stringify({ version: browser.version(), results, closes, statusBefore, statusAfter }, null, 1));
  await browser.close();
})();
EOF

# run_rooms <C|D> <シグナリングの URL> <静的サーバーの port> <server の log>
run_rooms() {
  local L=$1 SIG=$2 SP=$3 LOG=$4 C="senca-sdk-js-$$-$1" R="$W/result-$1.json" n r
  CONTAINERS+=("$C")
  timeout 200 docker run --rm --name "$C" --network host -v "$W:$W" "$IMG" bash -c \
    "node '$W/drive.cjs' 'http://127.0.0.1:$SP' '$R' 100000 \
       'room=demo&name=alice&simulcast=1&peers=2&secs=80&signal=$SIG' \
       'room=demo&name=bob&peers=2&secs=80&signal=$SIG' \
       'room=demo&name=carol&peers=2&secs=80&switch=1&target=alice&signal=$SIG'; chown $(id -u):$(id -g) '$R'" \
    >"$W/browser-$L.log" 2>&1 || true
  if [ ! -s "$R" ]; then fail "$L no browser result ($(tail -5 "$W/browser-$L.log"))"; return; fi
  echo "Chrome $(jq_ "$R" 'd["version"]')"
  for n in alice bob carol; do
    r="d['results']['$n']"
    if [ "$(jq_ "$R" "$r.get('ok')")" = True ]; then
      pass "$L $n: Senca.connect in $(jq_ "$R" "$r.get('connectMs')") ms, decoded $(jq_ "$R" "$r.get('decoded')"), #chat from $(jq_ "$R" "sorted($r.get('chatFrom'))"), direct pong from $(jq_ "$R" "sorted($r.get('directFrom'))")"
    else
      fail "$L $n: error=$(jq_ "$R" "$r.get('error')") decoded=$(jq_ "$R" "$r.get('decoded')") chat=$(jq_ "$R" "$r.get('chatFrom')") direct=$(jq_ "$R" "$r.get('directFrom')") errors=$(jq_ "$R" "$r.get('errors')")"
      echo "  log: $(jq_ "$R" "$r.get('log')")"
    fi
    others=$(jq_ "$R" "len([k for k,v in $r.get('decoded',{}).items() if k!='$n' and v>0])")
    [ "$others" = 2 ] && pass "$L $n: framesDecoded > 0 for both other participants (session.stats())" || fail "$L $n: decoded from $others participants"
    [ "$(jq_ "$R" "len($r.get('consoleErrors', []))")" = 0 ] && pass "$L $n: no console errors" || fail "$L $n: console errors $(jq_ "$R" "$r.get('consoleErrors')")"
    [ "$(jq_ "$R" "len($r.get('errors', []))")" = 0 ] || fail "$L $n: errors $(jq_ "$R" "$r.get('errors')")"
  done
  # 後から来た人の分は renegotiate で受けた（alice は最初 1 本の枠）: alice が bob と carol の両方を復号していれば足りている
  local pa; pa=$(jq_ "$R" "d['results']['alice'].get('publication')")
  [ "$(jq_ "$R" "[t for t in d['results']['alice'].get('publication') or [] if t['kind']=='video'][0]['rids']")" = "['r0', 'r1', 'r2']" ] \
    && pass "$L alice publishes simulcast r0/r1/r2 (publication $pa)" || fail "$L alice publication $pa"
  local hi lo lay np cl cs left after
  hi=$(jq_ "$R" "d['results']['carol'].get('widthHigh', 0)"); lo=$(jq_ "$R" "d['results']['carol'].get('widthLow', 0)")
  lay=$(jq_ "$R" "d['results']['carol'].get('layer')")
  echo "  alice outbound: $(jq_ "$R" "d['results']['alice'].get('outbound')") transport=$(jq_ "$R" "d['results']['alice'].get('transport')")"
  if [ "${hi:-0}" -gt "${lo:-0}" ] && [ "${lo:-0}" -gt 0 ]; then pass "$L simulcast: carol setLayer('r2') → ${hi}px, setLayer('r0') → ${lo}px (layer $lay)"
  else fail "$L simulcast: widthHigh=$hi widthLow=$lo layer=$lay"; fi
  np=$(jq_ "$R" "len(d['statusBefore'].get('participants', []))")
  cl=$(jq_ "$R" "len([p for p in d['statusBefore'].get('participants', []) if p.get('client')])")
  [ "$np" = 3 ] && pass "$L server stats: 3 participants, $cl with client getStats (reportStats via pong)" || fail "$L server stats: participants=$np $(jq_ "$R" "d['statusBefore']")"
  cs=$(jq_ "$R" "{k: v.get('reason') for k, v in d['closes'].items()}")
  left=$(jq_ "$R" "sorted(d['closes']['carol'].get('left') or [])")
  after=$(jq_ "$R" "len(d['statusAfter'].get('participants', [])) if isinstance(d['statusAfter'], dict) else -1")
  if [ "$cs" = "{'alice': 'normal', 'bob': 'normal', 'carol': 'normal'}" ] && [ "$left" = "['alice', 'bob']" ] && [ "$after" = 0 ]; then
    pass "$L close(): all closed normally, carol saw participant.left for $left, room empty afterwards"
  else
    fail "$L close(): closes=$cs carol saw left=$left participants after=$after"
  fi
  grep -q "connection.destroyed" "$LOG" && pass "$L server emitted connection.destroyed" || echo "  (no connection.destroyed in $L server log)"
}

echo "== C 部屋（crates/senca-rooms の rooms_server と Chrome 3 ページ）"
if [ -z "${SKIP_BUILD:-}" ]; then
  (cd "$ROOT" && "$CARGO" build -q -p senca-rooms --example rooms_server) >"$W/rooms-build.log" 2>&1 \
    || fail "C rooms_server を作れない: $(tail -3 "$W/rooms-build.log")"
fi
EX=$CARGO_TARGET_DIR/debug/examples/rooms_server
if [ -x "$EX" ]; then
  HP=$(port); UP=$(port); SP=$(port)
  "$EX" --http "127.0.0.1:$HP" --udp "$IP:$UP" >"$W/server.log" 2>&1 &
  PIDS+=("$!")
  if wait_http "http://127.0.0.1:$HP/token"; then
    python3 -I "$W/static.py" "$SP" "$SDK" "http://127.0.0.1:$HP" rooms_server >"$W/static-c.log" 2>&1 &
    PIDS+=("$!")
    wait_http "http://127.0.0.1:$SP/test/interop/page.html" && run_rooms C "ws://127.0.0.1:$HP/signaling" "$SP" "$W/server.log" || fail "C static server did not start"
  else
    fail "C rooms_server did not start ($(tail -3 "$W/server.log"))"
  fi
fi

echo "== D 部屋（本物の senca-node: 参加トークンは管理 API の POST /rooms/{id}/tokens、シグナリングは node の /signaling）"
if [ -x "$NODE_BIN" ]; then
  DH=$(port); DR=$(port); DU=$(port); SP2=$(port)
  "$NODE_BIN" --data-dir "$W/data-d" --http "127.0.0.1:$DH" --rtmp "127.0.0.1:$DR" --webrtc-udp "$IP:$DU" --public-rtmp-url "rtmp://127.0.0.1:$DR" \
    --public-http-url "http://127.0.0.1:$DH" >"$W/node-d.log" 2>&1 &
  PIDS+=("$!")
  if wait_http "http://127.0.0.1:$DH/readyz"; then
    python3 -I "$W/static.py" "$SP2" "$SDK" "http://127.0.0.1:$DH" node "$(cat "$W/data-d/control/bootstrap-admin-token")" >"$W/static-d.log" 2>&1 &
    PIDS+=("$!")
    wait_http "http://127.0.0.1:$SP2/test/interop/page.html" && run_rooms D "ws://127.0.0.1:$DH/signaling" "$SP2" "$W/node-d.log" || fail "D static server did not start"
  else
    fail "D senca-node did not start: $(tail -3 "$W/node-d.log")"
  fi
else
  echo "SKIP D (senca-node が無い)"
fi
summary
