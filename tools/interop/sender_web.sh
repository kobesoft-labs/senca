#!/usr/bin/env bash
# REQ-008 R8-10・REQ-009 R9-1: ブラウザの送信ページ（apps/sender-web）の試験。
#   A. 設定文字列・Link ヘッダーの単体試験と構文の検査（Docker の Node.js）
#   B. Docker の Chrome（偽のカメラ・マイク）でページを開き、WHIP（senca-webrtc の whip_server）へ送る:
#      送信中になる、getStats のビットレート・fps が出る、trickle の PATCH、停止で DELETE、受けた映像を FFmpeg で復号できる。
#      ページと WHIP は同じ origin にする（ノードが /sender/ で配る形）ため、小さな中継（静的ファイル + /whip の転送）を挟む。
#
# 使い方: tools/interop/sender_web.sh（KEEP=1 で作業場所を残す）
#   NODE_IMAGE（既定 node:22.23.4-bookworm-slim）・BROWSER_IMAGE（既定 ov-chrome:latest）が無ければその場面を飛ばす。
set -euo pipefail
source "$(dirname "$0")/lib.sh"
CARGO=${CARGO:-$HOME/.cargo/bin/cargo}
APP=$ROOT/apps/sender-web
NODE_IMAGE=${NODE_IMAGE:-node:22.23.4-bookworm-slim}
IMG=${BROWSER_IMAGE:-ov-chrome:latest}
mkdir -p "$HOME/.cache"
W=$(mktemp -d -p "$HOME/.cache" senca-sender-web.XXXXXX)
PIDS=(); CONTAINERS=()
cleanup() {
  local p c
  for p in "${PIDS[@]}"; do kill "$p" 2>/dev/null || true; done
  for c in "${CONTAINERS[@]}"; do docker stop -t 2 "$c" >/dev/null 2>&1 || true; done
  wait 2>/dev/null || true
  [ -n "${KEEP:-}" ] && echo "kept $W" || rm -rf "$W"
}
trap cleanup EXIT

# ---- A
if docker image inspect "$NODE_IMAGE" >/dev/null 2>&1; then
  if docker run --rm -v "$APP:/app:ro" -w /app "$NODE_IMAGE" sh -c 'for f in *.js; do node --check "$f" || exit 1; done; node --test test/config.test.mjs' >"$W/unit.log" 2>&1; then
    pass "A sender-web unit tests ($(grep -E '^# pass' "$W/unit.log"))"
  else
    fail "A sender-web unit tests: $(grep -E 'not ok|Error' "$W/unit.log" | head -3)"
  fi
else
  echo "SKIP A (no $NODE_IMAGE)"
fi

# ---- B
if ! docker image inspect "$IMG" >/dev/null 2>&1; then echo "SKIP B (no $IMG)"; summary; exit; fi
export CARGO_TARGET_DIR=${WEBRTC_TARGET:-$HOME/.cache/senca-target-sdk}
if ! (cd "$ROOT" && "$CARGO" build -q -p senca-webrtc --example whip_server) 2>"$W/build.log"; then
  echo "SKIP B (senca-webrtc の whip_server が作れない: $(head -3 "$W/build.log"))"; summary; exit
fi
IP=${HOST_IP:-$(hostname -I | awk '{print $1}')}
WP=$(port); UP=$(port); PX=$(port)
"$CARGO_TARGET_DIR/debug/examples/whip_server" --http "127.0.0.1:$WP" --udp "$IP:$UP" --out "$W/out" >"$W/whip.log" 2>&1 &
PIDS+=($!)
mkdir -p "$W/out"

# 中継: 静的ファイル（apps/sender-web）と /whip の転送、受けた要求の記録
cat > "$W/proxy.py" <<'EOF'
import http.server, sys, urllib.request, urllib.error, os, json
APP, UPSTREAM, LOG = sys.argv[2], sys.argv[3], sys.argv[4]
class H(http.server.SimpleHTTPRequestHandler):
    def __init__(self, *a, **k): super().__init__(*a, directory=APP, **k)
    def log_message(self, *a): pass
    def proxy(self):
        n = int(self.headers.get('Content-Length') or 0)
        body = self.rfile.read(n) if n else None
        hdr = {k: v for k, v in self.headers.items() if k.lower() in ('content-type', 'authorization', 'if-match')}
        with open(LOG, 'a') as f:
            f.write(json.dumps({'method': self.command, 'path': self.path, 'auth': hdr.get('Authorization', ''), 'ctype': hdr.get('Content-Type', ''), 'ifmatch': hdr.get('If-Match', ''), 'body': (body or b'').decode('utf-8', 'replace')[:20000]}) + '\n')
        req = urllib.request.Request(UPSTREAM + self.path, data=body, method=self.command, headers=hdr)
        try:
            r = urllib.request.urlopen(req, timeout=10); status = r.status
        except urllib.error.HTTPError as e:
            r = e; status = e.code
        data = r.read()
        self.send_response(status)
        for k, v in r.headers.items():
            if k.lower() in ('content-type', 'location', 'etag', 'link'): self.send_header(k, v)
        self.send_header('Content-Length', str(len(data))); self.end_headers(); self.wfile.write(data)
    def do_GET(self):
        if self.path == '/whip' or self.path.startswith('/whip/'): return self.proxy()
        return super().do_GET()
    do_POST = do_PATCH = do_DELETE = proxy
http.server.ThreadingHTTPServer(('127.0.0.1', int(sys.argv[1])), H).serve_forever()
EOF
python3 -I "$W/proxy.py" "$PX" "$APP" "http://127.0.0.1:$WP" "$W/requests.jsonl" & PIDS+=($!)
for _ in $(seq 100); do curl -sf "http://127.0.0.1:$PX/index.html" >/dev/null && curl -sf "http://127.0.0.1:$WP/status" >/dev/null && break; sleep 0.1; done

cat > "$W/drive.cjs" <<'EOF'
// node drive.cjs <url> <out.json>: ページを開いて「送信開始」を押し、送信中の統計を読んでから「停止」を押す
const fs = require('fs'); const path = require('path'); const { execSync } = require('child_process');
const [url, out, simulcast] = process.argv.slice(2);
function findPw() {
  try { return require.resolve('playwright-core'); } catch { /* 探す */ }
  const p = execSync("find / -path '*/node_modules/playwright-core/package.json' -not -path '/proc/*' 2>/dev/null | head -1").toString().trim();
  return path.dirname(p);
}
const { chromium } = require(findPw());
(async () => {
  const browser = await chromium.launch({ executablePath: '/usr/bin/google-chrome', headless: true,
    args: ['--use-fake-device-for-media-stream', '--use-fake-ui-for-media-stream'] });
  const page = await (await browser.newContext()).newPage();
  const logs = [];
  page.on('console', (m) => logs.push(m.type() + ': ' + m.text()));
  page.on('pageerror', (e) => logs.push('pageerror: ' + e.message));
  const r = { ok: false };
  try {
    await page.goto(url);
    await page.selectOption('#resolution', simulcast ? '1280x720' : '640x360');
    if (simulcast) await page.check('#simulcast');
    await page.click('#start');
    await page.waitForFunction(() => document.getElementById('state').textContent === '送信中', null, { timeout: 20000 });
    r.connected = true;
    await page.waitForTimeout(6000);
    r.stats = await page.evaluate(() => Object.fromEntries([...document.querySelectorAll('#stats div')].map((d) => [d.querySelector('dt').textContent, d.querySelector('dd').textContent])));
    await page.click('#stop');
    await page.waitForFunction(() => document.getElementById('state').textContent === '停止', null, { timeout: 10000 });
    r.ok = true;
  } catch (e) {
    r.error = e.message;
    r.state = await page.evaluate(() => document.getElementById('state').textContent + ' / ' + document.getElementById('message').textContent).catch(() => '');
  }
  r.version = browser.version();
  r.consoleErrors = logs.filter((l) => /^(error|pageerror)/.test(l) && !/status of 404/.test(l));
  fs.writeFileSync(out, JSON.stringify(r, null, 1));
  await browser.close();
})();
EOF
CFG=$(python3 -c "import urllib.parse,sys;print(urllib.parse.quote(sys.argv[1],safe=''))" "senca://ingest?v=1&proto=whip&url=http://127.0.0.1:$PX/whip&token=test-token-123")
C="senca-sender-web-$$"; CONTAINERS+=("$C")
timeout 120 docker run --rm --name "$C" --network host -v "$W:$W" "$IMG" bash -c \
  "node '$W/drive.cjs' 'http://127.0.0.1:$PX/index.html?config=$CFG' '$W/result.json'; chown $(id -u):$(id -g) '$W/result.json'" >"$W/browser.log" 2>&1 || true
[ -s "$W/result.json" ] || echo '{"ok":false,"error":"no result"}' > "$W/result.json"
jq_() { python3 -I -c "import json,sys;d=json.load(open(sys.argv[1]));print(eval(sys.argv[2]))" "$1" "$2"; }
R=$W/result.json
[ "$(jq_ "$R" 'd["ok"]')" = True ] && pass "B Chrome $(jq_ "$R" 'd["version"]'): 送信中 → 停止 (stats $(jq_ "$R" 'd["stats"]'))" \
  || fail "B browser: $(jq_ "$R" 'd.get("error")') $(jq_ "$R" 'd.get("state")')"
python3 -I - "$R" <<'PY' && pass "B getStats shows video bitrate and fps" || fail "B stats empty"
import json,sys,re
s=json.load(open(sys.argv[1])).get('stats') or {}
v=s.get('映像',''); fps=s.get('fps','—')
sys.exit(0 if re.search(r'[1-9]', v) and fps not in ('—','0') else 1)
PY
[ "$(jq_ "$R" 'len(d["consoleErrors"])')" = 0 ] && pass "B no console errors" || fail "B console errors: $(jq_ "$R" 'd["consoleErrors"]')"
python3 -I - "$W/requests.jsonl" <<'PY' && pass "B WHIP: POST application/sdp + Bearer, trickle PATCH (sdpfrag, If-Match), DELETE" || fail "B WHIP requests: $(cut -c1-200 "$W/requests.jsonl" | head -5)"
import json,sys
rs=[json.loads(l) for l in open(sys.argv[1])]
post=[r for r in rs if r['method']=='POST']; patch=[r for r in rs if r['method']=='PATCH']; dele=[r for r in rs if r['method']=='DELETE']
ok = len(post)==1 and post[0]['ctype']=='application/sdp' and post[0]['auth']=='Bearer test-token-123' and post[0]['body'].startswith('v=0')
ok = ok and len(dele)==1 and dele[0]['path'].startswith('/whip/') and dele[0]['auth']=='Bearer test-token-123'
# trickle は候補が POST の後に残っていれば送る（無ければ 0 回でもよい）。送ったなら形を確かめる
ok = ok and all(p['ctype']=='application/trickle-ice-sdpfrag' and p['ifmatch'] and 'a=ice-ufrag:' in p['body'] for p in patch)
sys.exit(0 if ok else 1)
PY
# サイマルキャスト（3 つの rid）
rm -f "$W/requests.jsonl"
C2="senca-sender-web-$$-s"; CONTAINERS+=("$C2")
timeout 120 docker run --rm --name "$C2" --network host -v "$W:$W" "$IMG" bash -c \
  "node '$W/drive.cjs' 'http://127.0.0.1:$PX/index.html?config=$CFG' '$W/result-s.json' 1; chown $(id -u):$(id -g) '$W/result-s.json'" >"$W/browser-s.log" 2>&1 || true
[ -s "$W/result-s.json" ] || echo '{"ok":false,"error":"no result"}' > "$W/result-s.json"
[ "$(jq_ "$W/result-s.json" 'd["ok"]')" = True ] && jq_ "$W/result-s.json" 'd["stats"]["映像"]' | grep -q "3 層" \
  && grep -q 'a=simulcast:send' "$W/requests.jsonl" && pass "B simulcast: offer has a=simulcast, 3 layers sent ($(jq_ "$W/result-s.json" 'd["stats"]'))" \
  || fail "B simulcast: $(jq_ "$W/result-s.json" 'd.get("error") or d.get("stats")')"
sleep 1
V=$(ls "$W"/out/*.ivf "$W"/out/*.h264 2>/dev/null | head -1 || true)
if [ -n "$V" ]; then
  n=$("$FFPROBE" -v error -count_packets -select_streams 0 -show_entries stream=nb_read_packets -of csv=p=0 "$V" 2>/dev/null | head -1)
  err=$("$FFMPEG" -v error -i "$V" -f null - 2>&1 | head -2 || true)
  [ -z "$err" ] && [ "${n:-0}" -ge 100 ] && pass "B received video decodes cleanly ($(basename "$V"), $n frames)" || fail "B received video n=$n err=${err:0:200}"
else
  fail "B no video received by whip_server"
fi
summary
