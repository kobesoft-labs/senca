#!/usr/bin/env bash
# リアルタイムの部屋（senca-rooms、ADR-0015）とブラウザー（Docker の Google Chrome ＋ playwright-core）の相互接続試験。
#
#   1 つの Chrome で 3 つのページ（alice: サイマルキャスト 3 層、bob、carol）が同じ部屋に WebSocket のシグナリング v1 で入る。
#   - 後から入った人のトラックのために先にいた人へ renegotiate（受信の枠を足して offer し直す）
#   - 各ページが他の 2 人の映像を復号する（getStats の framesDecoded > 0）
#   - DataChannel #chat の ping/pong が他の 2 人と往復する
#   - carol が alice の層を r2 → r0 と選ぶと、復号した映像の幅が小さくなる（キーフレームでの切り替え）
#   - サーバーの統計（REST の stats と同じ形）に 3 人と転送の量が出る
#
# 使い方: tools/interop/rooms.sh（KEEP=1 で作業場所を残す、BROWSER_IMAGE=... で Chrome のイメージを選ぶ）
# ポートは 51000〜51999 を使う。Chrome は --network host で動かす。
set -euo pipefail
source "$(dirname "$0")/lib.sh"
CARGO=${CARGO:-$HOME/.cargo/bin/cargo}
export CARGO_TARGET_DIR=${CARGO_TARGET_DIR:-$HOME/.cache/senca-target-rooms}
mkdir -p "$HOME/.cache"
W=$(mktemp -d -p "$HOME/.cache" senca-rooms-interop.XXXXXX)
PIDS=()
CONTAINERS=()
cleanup() {
  local p c
  for p in "${PIDS[@]}"; do kill "$p" 2>/dev/null || true; done
  for c in "${CONTAINERS[@]}"; do docker stop -t 2 "$c" >/dev/null 2>&1 || true; done
  wait 2>/dev/null || true
  if [ -n "${KEEP:-}" ]; then echo "作業ディレクトリ: $W"; else rm -rf "$W"; fi
}
trap cleanup EXIT

IMG=${BROWSER_IMAGE:-ov-chrome:latest}
if ! docker image inspect "$IMG" >/dev/null 2>&1; then
  echo "SKIP: Docker image $IMG がない"; exit 0
fi
[ -n "${SKIP_BUILD:-}" ] || (cd "$ROOT" && "$CARGO" build -q -p senca-rooms --example rooms_server)
EX=$CARGO_TARGET_DIR/debug/examples

IP=${HOST_IP:-$(hostname -I | awk '{print $1}')}
BASE=$((51000 + (RANDOM % 90) * 10))
HP=$((BASE + 1)); UP=$((BASE + 2))

# ------------------------------------------------------------------ 道具: 1 つの Chrome で複数のページ
cat > "$W/drive.cjs" <<'EOF'
// node drive.cjs <base> <out.json> <timeout_ms> <query>...
const fs = require('fs');
const path = require('path');
const { execSync } = require('child_process');
const [base, out, timeout, ...queries] = process.argv.slice(2);
function findPw() {
  try { return require.resolve('playwright-core'); } catch { /* 探す */ }
  const p = execSync("find / -path '*/node_modules/playwright-core/package.json' -not -path '/proc/*' 2>/dev/null | head -1").toString().trim();
  if (!p) throw new Error('playwright-core not found');
  return path.dirname(p);
}
const { chromium } = require(findPw());
(async () => {
  const exe = fs.existsSync('/usr/bin/google-chrome') ? '/usr/bin/google-chrome' : undefined;
  const browser = await chromium.launch({ executablePath: exe, headless: true, args: [
    '--use-fake-device-for-media-stream', '--use-fake-ui-for-media-stream', '--autoplay-policy=no-user-gesture-required'] });
  const ctx = await browser.newContext();
  const pages = [];
  const logs = {};
  for (const q of queries) {
    const page = await ctx.newPage();
    const name = new URLSearchParams(q).get('name');
    logs[name] = [];
    page.on('console', (m) => logs[name].push(m.type() + ': ' + m.text()));
    page.on('pageerror', (e) => logs[name].push('pageerror: ' + e.message));
    await page.goto(base + '/?' + q);
    pages.push([name, page]);
    await new Promise((r) => setTimeout(r, 1500));
  }
  const results = {};
  await Promise.all(pages.map(async ([name, page]) => {
    try {
      await page.waitForFunction(() => window.__result !== undefined, null, { timeout: Number(timeout) });
      results[name] = await page.evaluate(() => window.__result);
    } catch (e) {
      results[name] = { ok: false, error: 'driver: ' + e.message };
    }
    results[name].consoleErrors = logs[name].filter((l) => (l.startsWith('error') || l.startsWith('pageerror')) && !/status of 404/.test(l));
    results[name].log = logs[name].slice(-30);
  }));
  let status = null;
  try { status = await (await fetch(base + '/status?room=demo')).json(); } catch (e) { status = { error: String(e) }; }
  fs.writeFileSync(out, JSON.stringify({ version: browser.version(), results, status }, null, 1));
  await browser.close();
})();
EOF

jq_() { python3 -I -c "import json,sys;d=json.load(open(sys.argv[1]));print(eval(sys.argv[2]))" "$1" "$2"; }
wait_http() { local i; for i in $(seq 100); do curl -sf "$1" >/dev/null && return 0; sleep 0.1; done; return 1; }

echo "browser image: $IMG, host address: $IP, http $HP, udp $UP"
"$EX/rooms_server" --http "127.0.0.1:$HP" --udp "$IP:$UP" >"$W/server.log" 2>&1 &
PIDS+=("$!")
wait_http "http://127.0.0.1:$HP/token" || { fail "server did not start ($(tail -3 "$W/server.log"))"; summary; exit 1; }

C="senca-rooms-$$"
CONTAINERS+=("$C")
timeout 150 docker run --rm --name "$C" --network host -v "$W:$W" "$IMG" bash -c \
  "node '$W/drive.cjs' 'http://127.0.0.1:$HP' '$W/result.json' 90000 \
     'room=demo&name=alice&simulcast=1&peers=2&secs=60' \
     'room=demo&name=bob&peers=2&secs=60' \
     'room=demo&name=carol&peers=2&secs=60&switch=1&target=alice'; chown $(id -u):$(id -g) '$W/result.json'" \
  >"$W/browser.log" 2>&1 || true
R="$W/result.json"
if [ ! -s "$R" ]; then
  fail "no browser result ($(tail -5 "$W/browser.log"))"; summary; exit 1
fi
echo "Chrome $(jq_ "$R" 'd["version"]')"

for n in alice bob carol; do
  ok=$(jq_ "$R" "d['results']['$n'].get('ok')")
  dec=$(jq_ "$R" "d['results']['$n'].get('decoded')")
  pongs=$(jq_ "$R" "sorted(d['results']['$n'].get('pongsFrom', []))")
  if [ "$ok" = True ]; then
    pass "$n: decoded the other two ($dec), DataChannel round trip with $pongs"
  else
    fail "$n: ok=$ok decoded=$dec pongs=$pongs error=$(jq_ "$R" "d['results']['$n'].get('error')") errors=$(jq_ "$R" "d['results']['$n'].get('errors')")"
    echo "  log: $(jq_ "$R" "d['results']['$n'].get('log')")"
  fi
  others=$(jq_ "$R" "len([k for k,v in d['results']['$n'].get('decoded',{}).items() if k!='$n' and v>0])")
  [ "$others" = 2 ] && pass "$n: framesDecoded > 0 for both other participants" || fail "$n: decoded from $others participants"
  [ "$(jq_ "$R" "len(d['results']['$n'].get('consoleErrors', []))")" = 0 ] && pass "$n: no console errors" || fail "$n: console errors $(jq_ "$R" "d['results']['$n'].get('consoleErrors')")"
  [ "$(jq_ "$R" "len(d['results']['$n'].get('errors', []))")" = 0 ] || fail "$n: signaling errors $(jq_ "$R" "d['results']['$n'].get('errors')")"
done

# 先にいた人は後から来た人のために再交渉した
ra=$(jq_ "$R" "d['results']['alice'].get('renegotiations', 0)"); rb=$(jq_ "$R" "d['results']['bob'].get('renegotiations', 0)")
if [ "$ra" -ge 1 ] && [ "$rb" -ge 1 ]; then pass "renegotiation on later joins (alice $ra, bob $rb)"; else fail "renegotiations alice=$ra bob=$rb"; fi

# 層の切り替え
hi=$(jq_ "$R" "d['results']['carol'].get('widthHigh', 0)"); lo=$(jq_ "$R" "d['results']['carol'].get('widthLow', 0)")
lc=$(jq_ "$R" "d['results']['carol'].get('layerChanges')")
echo "  alice outbound: $(jq_ "$R" "d['results']['alice'].get('outbound')") availableOutgoingBitrate=$(jq_ "$R" "d['results']['alice'].get('availableOutgoingBitrate')")"
if [ "${hi:-0}" -gt "${lo:-0}" ] && [ "${lo:-0}" -gt 0 ]; then pass "simulcast layer selection: carol asked for r2 and decoded alice at ${hi}px, then r0 at ${lo}px; layer.changed $lc"
else fail "simulcast layer selection: widthHigh=$hi widthLow=$lo layer.changed=$lc"; fi

# サーバーの統計
np=$(jq_ "$R" "len(d['status'].get('participants', []))")
fb=$(jq_ "$R" "min(p['forwarded_bytes'] for p in d['status']['participants'])" 2>/dev/null || echo 0)
layers=$(jq_ "$R" "[ [ (l['rid'], l['bitrate_bps']) for t in p['published'] if t['kind']=='video' for l in t['layers']] for p in d['status']['participants']]" 2>/dev/null || echo "?")
echo "  published layers (rid, bit/s): $layers"
if [ "$np" = 3 ] && [ "${fb:-0}" -gt 100000 ]; then pass "server stats: 3 participants, each received >= $fb bytes via the SFU"
else fail "server stats: participants=$np min forwarded=$fb"; fi
grep -q "event connection.created" "$W/server.log" && pass "server emitted connection.created events" || fail "no connection.created event"
summary
