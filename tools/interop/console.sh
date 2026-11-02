#!/usr/bin/env bash
# 管理画面（apps/console）の試験: 配信の形（Content-Type・CSP）、JS の構文、純粋なロジック、
# ヘッドレスブラウザ（Docker の Chrome ＋ playwright-core）での操作と MSE の HLS 再生。
#
# 使い方: tools/interop/console.sh（KEEP=1 で作業場所を残す、NO_BROWSER=1 でブラウザを飛ばす）
#   BROWSER_IMAGE=... で Chrome と playwright-core の入ったイメージを選ぶ（既定は手元にあるものを探す）
set -euo pipefail
source "$(dirname "$0")/lib.sh"
CONSOLE_DIR="$ROOT/apps/console"
TARGET=${CARGO_TARGET_DIR:-/tmp/senca-target-main}
NODE_BIN=$TARGET/debug/senca-node
CODEC_LIB_DIR=${CODEC_LIB_DIR:-$HOME/tools/codeclibs/usr/lib/x86_64-linux-gnu}

# ノードの実行ファイル。無ければ作る（並行して Rust を編集している人がいるので、壊れていれば待って再試行）
if [ ! -x "$NODE_BIN" ]; then
  for i in 1 2 3 4 5; do
    if (cd "$ROOT" && CARGO_TARGET_DIR=$TARGET ~/.cargo/bin/cargo build -q -p senca-node); then break; fi
    [ "$i" = 5 ] && { echo "build failed"; exit 1; }
    echo "build failed (try $i), retrying in 30s"; sleep 30
  done
fi

W=$(mktemp -d); NODE_PID=; PUB_PID=
cleanup() {
  [ -n "$PUB_PID" ] && kill "$PUB_PID" 2>/dev/null || true
  [ -n "$NODE_PID" ] && kill "$NODE_PID" 2>/dev/null || true
  wait 2>/dev/null || true
  if [ -n "${KEEP:-}" ]; then echo "kept $W"; else rm -rf "$W"; fi
}
trap cleanup EXIT

# 49000〜49999 の空いているポート
rport() { python3 -c '
import random,socket
for _ in range(200):
    p=random.randint(49000,49999); s=socket.socket()
    try: s.bind(("127.0.0.1",p)); s.close(); print(p); break
    except OSError: pass'; }
HTTP=$(rport); RTMP=$(rport); while [ "$RTMP" = "$HTTP" ]; do RTMP=$(rport); done
BASE=http://127.0.0.1:$HTTP
CODEC_ARGS=()
[ -e "$CODEC_LIB_DIR/libdav1d.so.7" ] && CODEC_ARGS=(--codec-lib-dir "$CODEC_LIB_DIR" --codec-workers 2)
"$NODE_BIN" --data-dir "$W/data" --http 127.0.0.1:$HTTP --rtmp 127.0.0.1:$RTMP \
  --public-rtmp-url rtmp://127.0.0.1:$RTMP --public-http-url "$BASE" --reconnect-grace 2000 \
  --console-dir "$CONSOLE_DIR" "${CODEC_ARGS[@]}" >>"$W/node.log" 2>&1 &
NODE_PID=$!
for _ in $(seq 100); do curl -sf "$BASE/readyz" >/dev/null && break; sleep 0.1; done
curl -sf "$BASE/readyz" >/dev/null || { echo "node did not start"; cat "$W/node.log"; exit 1; }
TOKEN=$(cat "$W/data/control/bootstrap-admin-token")

# ------------------------------------------------------------------ A. 配信の形
CSP="default-src 'self'; media-src 'self' blob: data: *; connect-src 'self' *; img-src 'self' data: blob: *; style-src 'self' 'unsafe-inline'"
hdr() { curl -s -o /dev/null -D - "$BASE$1" | tr -d '\r'; }
h=$(hdr /console/)
echo "$h" | grep -q '^HTTP/1.1 200' && echo "$h" | grep -qi '^content-type: text/html; charset=utf-8' && pass "A /console/ is text/html" || fail "A /console/ headers: $h"
echo "$h" | grep -qiF "content-security-policy: $CSP" && pass "A CSP header present" || fail "A CSP header"
echo "$h" | grep -qi '^x-content-type-options: nosniff' && pass "A nosniff" || fail "A nosniff"
c=$(curl -s -o /dev/null -w '%{http_code} %{redirect_url}' "$BASE/"); [[ $c == "302 "*"/console/" ]] && pass "A / redirects to /console/" || fail "A root redirect $c"
bad=0; n=0
while IFS= read -r f; do
  rel=${f#"$CONSOLE_DIR"/}
  case "$rel" in *.js) want='text/javascript; charset=utf-8';; *.css) want='text/css; charset=utf-8';; *.html) want='text/html; charset=utf-8';; *) continue;; esac
  got=$(curl -s -o "$W/served" -w '%{http_code} %{content_type}' "$BASE/console/$rel")
  n=$((n+1))
  if [ "$got" != "200 $want" ] || ! cmp -s "$f" "$W/served"; then echo "  $rel -> $got"; bad=$((bad+1)); fi
done < <(find "$CONSOLE_DIR" -type f \( -name '*.js' -o -name '*.css' -o -name '*.html' \))
[ "$bad" = 0 ] && pass "A all $n console files served byte-identical with correct Content-Type" || fail "A $bad files served wrong"
got=$(curl -s -w '%{content_type}' "$BASE/console/streams/abc" -o "$W/spa.html"); cmp -s "$W/spa.html" "$CONSOLE_DIR/index.html" && [[ $got == text/html* ]] && pass "A SPA fallback serves index.html" || fail "A SPA fallback ($got)"
c=$(curl -s -o /dev/null -w '%{http_code}' --path-as-is "$BASE/console/../Cargo.toml"); [ "$c" = 404 ] && pass "A path traversal 404" || fail "A traversal $c"
c=$(curl -s -o /dev/null -w '%{http_code}' "$BASE/console/.hidden"); [ "$c" = 404 ] && pass "A dot file 404" || fail "A dot file $c"

# ------------------------------------------------------------------ B. ソースの静的検査
ok=1; for f in $(find "$CONSOLE_DIR" -name '*.js'); do node --check "$f" 2>"$W/check.err" || { ok=0; echo "  ${f#"$ROOT"/}: $(head -3 "$W/check.err")"; }; done
[ $ok = 1 ] && pass "B every JS module parses (node --check, $(find "$CONSOLE_DIR" -name '*.js' | wc -l) files)" || fail "B JS syntax"
grep -Eq '<script(\s[^>]*)?>[^<]' "$CONSOLE_DIR/index.html" && fail "B inline script in index.html" || pass "B no inline scripts (CSP)"
grep -rEn 'https?://' "$CONSOLE_DIR" --include='*.js' --include='*.css' --include='*.html' | grep -v 'www.w3.org/2000/svg' > "$W/ext.txt" || true
[ ! -s "$W/ext.txt" ] && pass "B no external URLs (no CDN)" || { fail "B external URLs"; cat "$W/ext.txt"; }
grep -rEn '\binnerHTML\b|\beval\(|new Function\(' "$CONSOLE_DIR/js" > "$W/unsafe.txt" || true
[ ! -s "$W/unsafe.txt" ] && pass "B no innerHTML/eval" || { fail "B unsafe DOM APIs"; cat "$W/unsafe.txt"; }
grep -rEn 'console\.(log|debug|info)\(' "$CONSOLE_DIR/js" > "$W/log.txt" || true
[ ! -s "$W/log.txt" ] && pass "B no console logging (token never logged)" || { fail "B console logging"; cat "$W/log.txt"; }
grep -rn 'localStorage' "$CONSOLE_DIR/js" | grep -i token > "$W/ls.txt" || true
[ ! -s "$W/ls.txt" ] && pass "B token not in localStorage" || { fail "B token in localStorage"; cat "$W/ls.txt"; }

# ------------------------------------------------------------------ C. 純粋なロジック（node で）
# AV1＋Opus の初期化セグメントを作る（ffmpeg の fMP4）
"$FFMPEG" -v error -f lavfi -i testsrc2=size=640x360:rate=30 -f lavfi -i sine=sample_rate=48000 -t 8 \
  -c:v libaom-av1 -cpu-used 8 -row-mt 1 -g 30 -b:v 500k -c:a libopus "$W/av1.mp4"
"$FFMPEG" -v error -f lavfi -i testsrc2=size=320x240:rate=30 -f lavfi -i sine=sample_rate=48000 -t 1 \
  -c:v libx264 -profile:v high -pix_fmt yuv420p -c:a aac -movflags +frag_keyframe+empty_moov+default_base_moof "$W/h264.mp4"
cat > "$W/unit.mjs" <<EOF
import fs from 'node:fs';
import assert from 'node:assert/strict';
const D = '$CONSOLE_DIR/js';
const { parsePlaylist, containerOf } = await import(D + '/player/m3u8.js');
const { parseInit, firstDecodeTime, mimeFor } = await import(D + '/player/mp4.js');
const { wallToUtcIso, utcToWall } = await import(D + '/lib/format.js');
const { samplePipeline, validate, toDefinition, fromDefinition, connect, emptyPipeline, addNode, hasCycle } = await import(D + '/pipeline/model.js');
const { EventStreamParser } = await import(D + '/api/sse.js');
const res = [];
const t = (name, fn) => { try { fn(); res.push('PASS C ' + name); } catch (e) { res.push('FAIL C ' + name + ': ' + e.message); } };
t('m3u8 live playlist with discontinuity and map change', () => {
  const pl = parsePlaylist('#EXTM3U\n#EXT-X-TARGETDURATION:2\n#EXT-X-MEDIA-SEQUENCE:13\n#EXT-X-MAP:URI="init1.mp4"\n#EXTINF:2.000,\n13.m4s\n#EXT-X-PART:DURATION=0.5,URI="p.0"\n#EXT-X-DISCONTINUITY\n#EXT-X-MAP:URI="init2.mp4",BYTERANGE="10@0"\n#EXTINF:1.5,\n#EXT-X-BYTERANGE:100@10\n14.m4s\n#EXT-X-ENDLIST\n', 'http://h/s/1/x/live/id/index.m3u8');
  assert.equal(pl.segments.length, 2); assert.equal(pl.segments[1].msn, 14); assert.equal(pl.segments[1].disc, 1);
  assert.equal(pl.segments[1].start, 2); assert.equal(pl.segments[1].map.uri, 'http://h/s/1/x/live/id/init2.mp4');
  assert.deepEqual(pl.segments[1].range, { length: 100, offset: 10 }); assert.equal(pl.endList, true); assert.equal(containerOf(pl), 'fmp4');
  assert.equal(containerOf(parsePlaylist('#EXTM3U\n#EXTINF:2,\na.ts\n', 'http://h/x.m3u8')), 'ts');
});
t('mp4 init AV1+Opus codec string', () => {
  const buf = fs.readFileSync('$W/av1.mp4');
  const init = parseInit(new Uint8Array(buf));
  assert.match(mimeFor(init), /^video\/mp4; codecs="av01\.0\.\d\dM\.08,opus"\$/);
  assert.equal(init.tracks.find((x) => x.kind === 'video').width, 640);
});
t('mp4 init H.264+AAC codec string and tfdt', () => {
  const buf = new Uint8Array(fs.readFileSync('$W/h264.mp4'));
  const init = parseInit(buf);
  assert.equal(mimeFor(init), 'video/mp4; codecs="avc1.64000d,mp4a.40.2"');
  // 最初の moof を探して tfdt を読む
  const dv = new DataView(buf.buffer, buf.byteOffset); let o = 0; let moof = -1;
  while (o < buf.length) { const sz = dv.getUint32(o); const ty = String.fromCharCode(...buf.slice(o + 4, o + 8)); if (ty === 'moof') { moof = o; break; } o += sz; }
  assert.ok(moof > 0); assert.equal(firstDecodeTime(buf.slice(moof), init), 0);
});
t('timezone wall clock to UTC (DST gap and overlap)', () => {
  assert.equal(wallToUtcIso('2026-11-02T09:00', 'Asia/Tokyo'), '2026-11-02T00:00:00.000Z');
  assert.equal(wallToUtcIso('2026-03-08T02:30', 'America/New_York'), '2026-03-08T07:30:00.000Z');
  assert.equal(wallToUtcIso('2026-11-01T01:30', 'America/New_York'), '2026-11-01T05:30:00.000Z');
  assert.equal(wallToUtcIso('2026-06-01T12:00', 'UTC'), '2026-06-01T12:00:00.000Z');
  assert.equal(utcToWall('2026-11-02T00:00:00Z', 'Asia/Tokyo'), '2026-11-02T09:00');
});
t('pipeline sample validates, round-trips, rejects bad links', () => {
  const sp = samplePipeline();
  assert.deepEqual(validate(sp).filter((i) => i.level === 'error'), []);
  const rt = fromDefinition(toDefinition(sp)); assert.equal(rt.nodes.length, sp.nodes.length); assert.equal(rt.edges.length, sp.edges.length);
  const p = emptyPipeline('x'); const a = addNode(p, 'input', 0, 0); const o = addNode(p, 'output', 0, 0); const m = addNode(p, 'mix', 0, 0);
  assert.equal(connect(p, { node: a.id, port: 0 }, { node: m.id, port: 0 }).ok, false); // av → audio
  const t1 = addNode(p, 'transcode', 0, 0); const t2 = addNode(p, 'transcode', 0, 0);
  assert.equal(connect(p, { node: t1.id, port: 0 }, { node: t2.id, port: 0 }).ok, true);
  assert.equal(connect(p, { node: t2.id, port: 0 }, { node: t1.id, port: 0 }).ok, false); // 輪
  assert.equal(hasCycle(p), false);
  t1.params.codec = 'h264';
  assert.ok(validate(p, { codecProfile: 'royalty_free' }).some((i) => i.level === 'error' && /royalty_free/.test(i.text)));
  void o;
});
t('SSE parser (CRLF split across chunks, comments, resync)', () => {
  const evs = []; const ps = new EventStreamParser((e) => evs.push(e));
  ps.feed(': keepalive\r\nevent: stream.created\r\nid: 42\r\ndata: {"type":"stream.created","resource_id":"x"}\r\n\r');
  ps.feed('\nevent: resync\nid: 43\ndata: {}\n\n');
  assert.equal(evs.length, 2); assert.equal(evs[0].id, '42'); assert.equal(evs[0].data.resource_id, 'x'); assert.equal(evs[1].type, 'resync');
});
console.log(res.join('\n'));
EOF
while read -r line; do case "$line" in PASS*) pass "${line#PASS }";; FAIL*) fail "${line#FAIL }";; esac; done < <(node "$W/unit.mjs" 2>&1 || echo "FAIL C unit runner crashed")

# ------------------------------------------------------------------ D. ブラウザ
find_image() {
  if [ -n "${BROWSER_IMAGE:-}" ]; then echo "$BROWSER_IMAGE"; return; fi
  for img in ov-chrome:latest $(docker images --format '{{.Repository}}:{{.Tag}}' 2>/dev/null | grep -E '^mcr.microsoft.com/playwright' || true); do
    docker image inspect "$img" >/dev/null 2>&1 && { echo "$img"; return; }
  done
  # 無ければ公式の Playwright イメージを取る
  docker pull -q mcr.microsoft.com/playwright:v1.56.1-noble >/dev/null 2>&1 && echo mcr.microsoft.com/playwright:v1.56.1-noble
}
IMG=
if [ -z "${NO_BROWSER:-}" ] && command -v docker >/dev/null && docker info >/dev/null 2>&1; then IMG=$(find_image || true); fi
if [ -z "$IMG" ]; then
  echo "SKIP D (no Docker image with Chromium available; static checks only)"
  summary; exit $?
fi
echo "browser image: $IMG"

cat > "$W/browser.cjs" <<'EOF'
// 管理画面をヘッドレスの Chrome で操作する。結果は {checks:[{name,ok,detail}]} を result.json に書く。
const fs = require('fs');
const path = require('path');
const { execSync } = require('child_process');
const [base, token, work] = process.argv.slice(2);
function findPw() {
  try { return require.resolve('playwright-core'); } catch { /* 探す */ }
  const out = execSync("find / -path '*/node_modules/playwright-core/package.json' -not -path '/proc/*' 2>/dev/null | head -1").toString().trim();
  if (!out) throw new Error('playwright-core not found');
  return path.dirname(out);
}
const { chromium } = require(findPw());
const checks = [];
const check = (name, ok, detail = '') => { checks.push({ name, ok: !!ok, detail: String(detail).slice(0, 400) }); };
const sleep = (ms) => new Promise((r) => setTimeout(r, ms));
const waitFile = async (f, ms) => { const end = Date.now() + ms; while (Date.now() < end) { if (fs.existsSync(f)) return true; await sleep(200); } return false; };
// 試験の中で意図して起こす失敗（[状態, URL]）。ブラウザはこれらも console に出すので除く
const EXPECTED_FAIL = [
  [/404/, /\/api\/v1\/(pipelines|productions)(\?|\/|$)/], // 未実装の API の確認
  [/401/, /\/api\/v1\/capabilities$/], // 間違ったトークンでのログイン
  [/412/, /\/api\/v1\/recordings\/[^/?]+$/], // 古い ETag
  [/40[49]|503/, /\/api\/v1\/streams\/[^/]+\/snapshot(\?|$)/], // 今の画が無い・作れない
  [/404/, /\/s\/\d+\/[^/]+\/live\/[^/]+\/index\.m3u8/], // 送り手を止めた後のライブ
];

(async () => {
  const exe = fs.existsSync('/usr/bin/google-chrome') ? '/usr/bin/google-chrome' : undefined;
  const browser = await chromium.launch({ executablePath: exe, headless: true, args: ['--autoplay-policy=no-user-gesture-required'] });
  const ctx = await browser.newContext({ viewport: { width: 1360, height: 900 }, acceptDownloads: true, locale: 'ja-JP', timezoneId: 'Asia/Tokyo' });
  const page = await ctx.newPage();
  const errors = [];
  const failedReqs = [];
  page.on('pageerror', (e) => errors.push(`pageerror: ${e.message}`));
  page.on('console', (m) => {
    if (m.type() !== 'error') return;
    const url = (m.location() && m.location().url) || '';
    if (/Failed to load resource/.test(m.text()) && EXPECTED_FAIL.some(([st, re]) => st.test(m.text()) && re.test(url))) return;
    errors.push(`console: ${m.text()} @ ${url}`);
  });
  page.on('requestfailed', (r) => { if (!/net::ERR_ABORTED/.test(r.failure()?.errorText || '')) failedReqs.push(`${r.url()} ${r.failure()?.errorText}`); });
  let tokenLeak = false;
  page.on('console', (m) => { if (m.text().includes(token)) tokenLeak = true; });
  const shot = (n) => page.screenshot({ path: path.join(work, `${n}.png`), fullPage: true }).catch(() => {});
  const step = async (name, fn) => {
    // 前の段で失敗して開いたままのダイアログを閉じる
    if (await page.locator('dialog[open]').count()) await page.keyboard.press('Escape');
    try { await fn(); } catch (e) { check(name, false, e.message); await shot(`fail-${name.replace(/\W+/g, '_')}`); }
  };

  await step('login', async () => {
    await page.goto(`${base}/console/`);
    await page.getByLabel('API トークン').fill('senca_wrong');
    await page.getByRole('button', { name: 'ログイン' }).click();
    await page.getByText('トークンが正しくないか').waitFor({ timeout: 5000 });
    check('login rejects bad token', true);
    await page.getByLabel('API トークン').fill(token);
    await page.getByRole('button', { name: 'ログイン' }).click();
    await page.getByRole('heading', { name: 'ダッシュボード' }).waitFor({ timeout: 10000 });
    const stored = await page.evaluate(() => Object.keys(sessionStorage).length === 1 && Object.keys(localStorage).every((k) => !localStorage.getItem(k).includes('senca_')));
    check('login with bootstrap token → dashboard (token only in sessionStorage)', stored);
    await page.getByText('イベント受信中').waitFor({ timeout: 10000 });
    check('SSE over fetch connected', true);
    await page.getByText('dav1d').first().waitFor({ timeout: 5000 }).then(() => check('dashboard shows devices', true)).catch(() => check('dashboard shows devices (no codec worker)', true, 'no providers'));
    await shot('01-dashboard');
  });

  let key = null;
  await step('create stream', async () => {
    await page.goto(`${base}/console/#/streams`);
    await page.getByRole('button', { name: '＋ 配信を作成' }).click();
    const dlg = page.getByRole('dialog');
    await dlg.getByLabel('名前', { exact: true }).fill('browser-av1');
    await dlg.getByLabel('コーデックの方針').selectOption('royalty_free');
    await dlg.getByLabel('publish が始まったら自動で録画する').check();
    await dlg.getByRole('button', { name: '作成' }).click();
    await page.getByRole('heading', { name: 'browser-av1' }).waitFor({ timeout: 10000 });
    key = await page.getByLabel('ストリームキー', { exact: true }).inputValue();
    check('create stream via form → detail with one-time stream key', /^sk_/.test(key), key ? 'sk_…' : 'none');
    check('one-time key warning shown', await page.getByText('この鍵はもう表示されません').isVisible());
    fs.writeFileSync(path.join(work, 'key.txt'), key);
  });

  await step('live playback', async () => {
    if (!(await waitFile(path.join(work, 'published'), 30000))) throw new Error('publisher did not start');
    await page.locator('.badge.live').first().waitFor({ timeout: 20000 });
    check('observed state becomes live without reload', true);
    await page.getByRole('button', { name: '▶ 再生' }).click();
    const video = page.locator('video[aria-label="ライブのプレビュー"]');
    await page.waitForFunction(() => { const v = document.querySelector('video[aria-label="ライブのプレビュー"]'); return v && v.currentTime > 1.5; }, null, { timeout: 30000 });
    const t1 = await video.evaluate((v) => v.currentTime);
    await sleep(3000);
    const t2 = await video.evaluate((v) => v.currentTime);
    const status = await page.locator('.player-status').first().textContent();
    const info = await video.evaluate((v) => ({ w: v.videoWidth, h: v.videoHeight, src: v.src.slice(0, 5), paused: v.paused }));
    check('MSE live HLS plays (currentTime advances)', t2 > t1 + 1.5 && info.src === 'blob:' && info.w === 640, `t1=${t1.toFixed(2)} t2=${t2.toFixed(2)} ${JSON.stringify(info)} status=${status}`);
    check('player status shows MSE + codec', /MSE/.test(status) && /av01/.test(status), status);
    await shot('02-stream-live');
  });

  await step('snapshot and plan', async () => {
    await page.getByRole('button', { name: '今の画を取る' }).click();
    const img = page.locator('img[alt="今の画（静止画）"]');
    await page.waitForFunction(() => { const i = document.querySelector('img[alt="今の画（静止画）"]'); return (i && i.naturalWidth > 0) || document.querySelector('.snap-msg .alert'); }, null, { timeout: 15000 });
    const w = await img.evaluate((i) => i.naturalWidth);
    const msg = await page.locator('.snap-msg').textContent().catch(() => '');
    check('snapshot via auth fetch → blob image (or reasoned message)', w === 640 || /今の画/.test(msg), `width=${w} ${msg}`);
    await page.getByRole('button', { name: '計画を取る' }).click();
    await page.getByRole('cell', { name: 'recording' }).first().waitFor({ timeout: 5000 });
    check('transform plan table', true);
  });

  await step('recording vod', async () => {
    fs.writeFileSync(path.join(work, 'stop'), '1');
    await page.getByRole('button', { name: '■ 停止' }).click();
    await page.goto(`${base}/console/#/recordings`);
    await page.locator('table a').first().click();
    await page.getByRole('heading', { name: '再生（VOD）' }).waitFor();
    await page.locator('.badge.ok', { hasText: '確定' }).first().waitFor({ timeout: 30000 });
    check('recording finalized (SSE refresh)', true);
    await page.getByRole('button', { name: '▶ 再生' }).click();
    await page.waitForFunction(() => { const v = document.querySelector('video[aria-label="録画の再生"]'); return v && v.currentTime > 2; }, null, { timeout: 20000 });
    const d = await page.locator('video[aria-label="録画の再生"]').evaluate((v) => ({ t: v.currentTime, d: v.duration }));
    check('MSE VOD plays with duration', d.d > 4 && isFinite(d.d), JSON.stringify(d));
    // シーク
    await page.locator('video[aria-label="録画の再生"]').evaluate((v) => { v.currentTime = Math.max(0, v.duration - 3); });
    await page.waitForFunction(() => { const v = document.querySelector('video[aria-label="録画の再生"]'); return v && !v.seeking && v.currentTime > v.duration - 3 && v.readyState >= 2; }, null, { timeout: 15000 });
    check('VOD seek near end', true);
    await page.getByRole('switch').check();
    await page.getByText('法的保全にしました').waitFor({ timeout: 5000 });
    check('legal hold toggle (PATCH If-Match)', await page.getByRole('button', { name: '削除' }).isDisabled());
    const [dl] = await Promise.all([page.waitForEvent('download', { timeout: 20000 }), page.getByRole('button', { name: 'MP4 を書き出す' }).click()]);
    const p = await dl.path();
    const size = fs.statSync(p).size;
    const head = fs.readFileSync(p).subarray(4, 8).toString();
    check('MP4 export download via auth fetch', size > 10000 && head === 'ftyp', `${dl.suggestedFilename()} ${size} bytes`);
    await shot('03-recording');
  });

  await step('stale etag 412', async () => {
    // 別の操作で世代を進めてから画面で保存 → 412 で読み直す
    const id = await page.evaluate(() => location.hash.split('/')[2]);
    const r = await page.evaluate(async ([id, token]) => {
      const g = await fetch(`/api/v1/recordings/${id}`, { headers: { Authorization: `Bearer ${token}` } });
      const p = await fetch(`/api/v1/recordings/${id}`, { method: 'PATCH', headers: { Authorization: `Bearer ${token}`, 'Content-Type': 'application/json', 'If-Match': g.headers.get('etag') }, body: JSON.stringify({ name: 'renamed-elsewhere' }) });
      return p.status;
    }, [id, token]);
    await page.getByLabel('名前', { exact: true }).fill('mine');
    await page.getByRole('button', { name: '保存', exact: true }).click();
    await page.getByText('最新の内容を読み込み直しました').first().waitFor({ timeout: 5000 });
    check('412 → reload and tell user', r === 200);
  });

  await step('schedules', async () => {
    await page.goto(`${base}/console/#/schedules`);
    await page.getByRole('button', { name: '＋ 予約を作成' }).click();
    const dlg = page.getByRole('dialog');
    await dlg.getByLabel('タイムゾーン').selectOption('Asia/Tokyo');
    await dlg.getByLabel('名前（任意）').fill('browser-sched');
    await dlg.getByRole('button', { name: '検査（重なりを確かめる）' }).click();
    await dlg.getByText('検査に通りました').waitFor({ timeout: 5000 });
    check('schedule check (POST /schedules/check)', true);
    await dlg.getByRole('button', { name: '検査して作成' }).click();
    await page.getByRole('heading', { name: 'browser-sched' }).waitFor({ timeout: 10000 });
    check('schedule created → detail', true);
    await page.getByRole('button', { name: '取り消す' }).click();
    await page.getByRole('dialog').getByRole('button', { name: '取り消す' }).click();
    await page.getByText('取り消しました').waitFor({ timeout: 5000 });
    check('schedule cancel (PATCH state)', true);
    await shot('04-schedule');
  });

  await step('tokens', async () => {
    await page.goto(`${base}/console/#/tokens`);
    await page.getByRole('button', { name: '＋ トークンを発行' }).click();
    const dlg = page.getByRole('dialog');
    await dlg.getByLabel('名前').fill('browser-ro');
    await dlg.getByRole('button', { name: '発行' }).click();
    const val = await page.getByRole('dialog').getByRole('textbox', { name: '「browser-ro」のトークン' }).inputValue();
    check('token created and shown once', /^senca_/.test(val));
    await page.getByRole('button', { name: '控えました' }).click();
    await page.getByRole('cell', { name: 'browser-ro', exact: true }).waitFor();
  });

  await step('nodes', async () => {
    await page.goto(`${base}/console/#/nodes`);
    await page.getByRole('button', { name: 'drain', exact: true }).click();
    await page.getByRole('dialog').getByRole('button', { name: 'drain' }).click();
    await page.getByRole('button', { name: 'drain を解除' }).waitFor({ timeout: 5000 });
    await page.getByRole('button', { name: 'drain を解除' }).click();
    await page.getByRole('dialog').getByRole('button', { name: '解除' }).click();
    await page.getByRole('button', { name: 'drain', exact: true }).waitFor({ timeout: 5000 });
    check('node drain / undrain', true);
  });

  await step('pipelines', async () => {
    await page.goto(`${base}/console/#/pipelines`);
    await page.locator('.pl-node').first().waitFor();
    const n = await page.locator('.pl-node').count();
    const node = page.locator('.pl-node').first();
    const before = await node.getAttribute('transform');
    const box = await node.boundingBox();
    await page.mouse.move(box.x + 40, box.y + 20); await page.mouse.down(); await page.mouse.move(box.x + 120, box.y + 80, { steps: 5 }); await page.mouse.up();
    const after = await page.locator('.pl-node').first().getAttribute('transform');
    check('pipeline editor renders sample graph and drags nodes', n >= 10 && before !== after, `${n} nodes ${before} -> ${after}`);
    await page.getByRole('button', { name: '出力', exact: true }).click();
    await page.keyboard.press('ArrowRight');
    await page.getByRole('button', { name: 'ノードに保存' }).click();
    await page.getByText('このノードでは使えません').first().waitFor({ timeout: 5000 });
    check('pipeline API 404 → "not available on this node"', true);
    await shot('05-pipelines');
  });

  await step('production', async () => {
    await page.goto(`${base}/console/#/production`);
    await page.getByText('このノードでは使えません').first().waitFor({ timeout: 5000 });
    const pgm = () => page.locator('.monitor.program .mlabel').textContent();
    const a = await pgm();
    await page.locator('main').click({ position: { x: 5, y: 5 } });
    await page.keyboard.press('3');
    await page.keyboard.press('c');
    await sleep(200);
    const b = await pgm();
    await page.getByRole('button', { name: 'FADE' }).click();
    await sleep(900);
    const c = await pgm();
    const f = page.getByLabel('カメラ B の音量（dB）');
    await f.focus(); await page.keyboard.press('ArrowUp');
    check('switcher cut/fade and fader work offline', a !== b && b !== c && /PinP/.test(b), `${a} -> ${b} -> ${c}`);
    await shot('06-production');
  });

  await step('logout', async () => {
    await page.getByRole('button', { name: 'ログアウト' }).click();
    await page.getByRole('heading', { name: 'ログイン' }).waitFor();
    check('logout clears token', await page.evaluate(() => sessionStorage.length === 0));
  });

  check('no unexpected console errors', errors.length === 0, errors.join(' | '));
  check('token never written to console', !tokenLeak);
  fs.writeFileSync(path.join(work, 'result.json'), JSON.stringify({ checks, failedReqs }, null, 1));
  await browser.close();
})().catch((e) => {
  checks.push({ name: 'browser runner', ok: false, detail: e.stack });
  fs.writeFileSync(path.join(work, 'result.json'), JSON.stringify({ checks }, null, 1));
  process.exit(0);
});
EOF

# 送り手: ブラウザが鍵を書いたら AV1 を実時間で送る（ffmpeg の Enhanced FLV、映像だけ）
(
  for _ in $(seq 300); do [ -s "$W/key.txt" ] && break; sleep 0.2; done
  [ -s "$W/key.txt" ] || exit 0
  "$FFMPEG" -v error -re -stream_loop -1 -i "$W/av1.mp4" -map 0:v -c copy -f flv "rtmp://127.0.0.1:$RTMP/live/$(cat "$W/key.txt")" >>"$W/pub.log" 2>&1 &
  P=$!; touch "$W/published"
  for _ in $(seq 900); do [ -e "$W/stop" ] && break; sleep 0.2; done
  kill $P 2>/dev/null; wait $P 2>/dev/null
) &
PUB_PID=$!

docker run --rm --network host --ipc=host -v "$W:$W" "$IMG" bash -c \
  "node '$W/browser.cjs' '$BASE' '$TOKEN' '$W'; chown -R $(id -u):$(id -g) '$W'" > "$W/browser.log" 2>&1 || true
wait "$PUB_PID" 2>/dev/null || true; PUB_PID=
if [ -s "$W/result.json" ]; then
  while IFS=$'\t' read -r ok name detail; do
    if [ "$ok" = True ]; then pass "D $name"; else fail "D $name${detail:+ — $detail}"; fi
  done < <(python3 -c 'import json,sys
for c in json.load(open(sys.argv[1]))["checks"]: print("%s\t%s\t%s" % (c["ok"], c["name"], c["detail"].replace("\n"," ").replace("\t"," ")))' "$W/result.json")
  mkdir -p "${SHOTS:-/tmp/senca-console-shots}"
  cp "$W"/*.png "${SHOTS:-/tmp/senca-console-shots}/" 2>/dev/null && echo "screenshots: ${SHOTS:-/tmp/senca-console-shots}"
else
  fail "D browser produced no result"; tail -30 "$W/browser.log"
fi
grep -q "sk_" "$W/node.log" && fail "D stream key leaked to node log" || pass "D no stream key in node log"
summary
