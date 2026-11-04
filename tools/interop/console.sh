#!/usr/bin/env bash
# 管理画面（apps/console）の試験: 配信の形（Content-Type・CSP）、JS の構文、純粋なロジック、
# ヘッドレスブラウザ（Docker の Chrome ＋ playwright-core）での操作と再生:
#   MSE の HLS 再生・録画・予約・トークン・ノード、秘密値・入力・出力・字幕の設定（CRUD）、
#   WHEP（WebRTC）のプレビュー（H.264 の RTMP）、パイプラインの編集・保存・検証（VP9/AV1）・開始、
#   スイッチャーの操作の往復（2 つの VP9 の入力を合成）、このブラウザからの WHIP 送信（Chrome の偽のカメラ）。
#
# 使い方: tools/interop/console.sh（KEEP=1 で作業場所を残す、NO_BROWSER=1 でブラウザを飛ばす）
#   BROWSER_IMAGE=... で Chrome と playwright-core の入ったイメージを選ぶ（既定は ov-chrome:latest、無ければ手元にあるものを探す）
set -euo pipefail
source "$(dirname "$0")/lib.sh"
CONSOLE_DIR="$ROOT/apps/console"
TARGET=${CARGO_TARGET_DIR:-/tmp/senca-target-main}
NODE_BIN=$TARGET/debug/senca-node
CODEC_LIB_DIR=${CODEC_LIB_DIR:-$HOME/tools/codeclibs/usr/lib/x86_64-linux-gnu}

# 実行ファイル。無ければ作る（並行して Rust を編集している人がいるので、壊れていれば待って再試行）
if [ ! -x "$NODE_BIN" ] || [ ! -x "$TARGET/debug/mp4-to-flv" ] || [ ! -x "$TARGET/debug/examples/rtmp_push" ]; then
  for i in 1 2 3 4 5; do
    if (cd "$ROOT" && CARGO_TARGET_DIR=$TARGET ~/.cargo/bin/cargo build -q -p senca-node -p senca-codec-worker -p senca-devtools \
        && CARGO_TARGET_DIR=$TARGET ~/.cargo/bin/cargo build -q -p senca-rtmp --examples); then break; fi
    [ "$i" = 5 ] && { echo "build failed"; exit 1; }
    echo "build failed (try $i), retrying in 30s"; sleep 30
  done
fi

mkdir -p "$HOME/.cache"
W=$(mktemp -d -p "$HOME/.cache" senca-console.XXXXXX); NODE_PID=; PUB_PID=; FEED_PIDS=()
cleanup() {
  touch "$W/stop-all" 2>/dev/null || true
  local p
  for p in "${FEED_PIDS[@]}"; do kill "$p" 2>/dev/null || true; done
  [ -n "$PUB_PID" ] && kill "$PUB_PID" 2>/dev/null || true
  [ -n "$NODE_PID" ] && kill "$NODE_PID" 2>/dev/null || true
  docker rm -f senca-console-browser-$$ >/dev/null 2>&1 || true
  wait 2>/dev/null || true
  if [ -n "${KEEP:-}" ]; then echo "kept $W"; else rm -rf "$W"; fi
}
trap cleanup EXIT

# 49000〜49999 の空いているポート（TCP と UDP の両方）。SENCA_PORT_MIN・SENCA_PORT_MAX で範囲を変えられる
USED=" "
rport() { local p; while :; do p=$(python3 -c '
import os,random,socket
lo=int(os.environ.get("SENCA_PORT_MIN","49000")); hi=int(os.environ.get("SENCA_PORT_MAX","49999"))
for _ in range(200):
    p=random.randint(lo,hi); s=socket.socket(); u=socket.socket(socket.AF_INET,socket.SOCK_DGRAM)
    try: s.bind(("127.0.0.1",p)); u.bind(("127.0.0.1",p)); s.close(); u.close(); print(p); break
    except OSError: pass'); case "$USED" in *" $p "*) ;; *) USED="$USED$p "; echo "$p"; return;; esac; done; }
HTTP=$(rport); RTMP=$(rport); UDP=$(rport); RTSP=$(rport)
BASE=http://127.0.0.1:$HTTP
CODEC_ARGS=()
[ -e "$CODEC_LIB_DIR/libdav1d.so.7" ] && CODEC_ARGS=(--codec-lib-dir "$CODEC_LIB_DIR" --codec-workers 2)
"$NODE_BIN" --data-dir "$W/data" --http 127.0.0.1:$HTTP --rtmp 127.0.0.1:$RTMP --webrtc-udp 127.0.0.1:$UDP \
  --rtsp 127.0.0.1:$RTSP --public-rtsp-url rtsp://127.0.0.1:$RTSP \
  --public-rtmp-url rtmp://127.0.0.1:$RTMP --public-http-url "$BASE" --reconnect-grace 2000 --egress-allow 127.0.0.0/8 \
  --console-dir "$CONSOLE_DIR" "${CODEC_ARGS[@]}" >>"$W/node.log" 2>&1 &
NODE_PID=$!
for _ in $(seq 100); do curl -sf "$BASE/readyz" >/dev/null && break; sleep 0.1; done
curl -sf "$BASE/readyz" >/dev/null || { echo "node did not start"; cat "$W/node.log"; exit 1; }
TOKEN=$(cat "$W/data/control/bootstrap-admin-token")
api() { local m=$1 p=$2; shift 2; curl -s -X "$m" -H "Authorization: Bearer $TOKEN" -H 'Content-Type: application/json' "$BASE/api/v1$p" "$@"; }

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
const { sampleGraph, validate, toGraph, fromGraph, connect, emptyPipeline, addNode, hasCycle, outType, sinkOrigins } = await import(D + '/pipeline/model.js');
const { issuesOf, runtimeOf } = await import(D + '/api/pipelines.js');
const { parseSubtitleMedia } = await import(D + '/views/stream-io.js');
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
t('pipeline graph round-trips (senca-pipeline JSON) and port types', () => {
  const canon = (v) => JSON.stringify(v, (k, x) => (x && typeof x === 'object' && !Array.isArray(x) ? Object.fromEntries(Object.entries(x).sort()) : x));
  const g = sampleGraph({ a: 'A', b: 'B', out: 'O' });
  const pl = fromGraph(g, { name: 'x' });
  assert.equal(canon(toGraph(pl)), canon(g));
  assert.deepEqual(validate(pl, { royaltyFree: true }).filter((i) => i.level === 'error'), []);
  pl.nodes.find((n) => n.name === 'venc').p.codec = 'h264';
  assert.ok(validate(pl, { royaltyFree: true }).some((i) => i.level === 'error' && /royalty_free/.test(i.text)));
  const p = emptyPipeline('y'); const s = addNode(p, 'source', 0, 0); s.p.stream = 'A';
  const d = addNode(p, 'decode', 0, 0); const e = addNode(p, 'encode', 0, 0); const o = addNode(p, 'sink', 0, 0); o.p.stream = 'O';
  assert.equal(connect(p, { node: s.id, port: 0 }, { node: e.id, port: 0 }).ok, false); // 符号化済み → encode は不可
  assert.equal(connect(p, { node: s.id, port: 0 }, { node: d.id, port: 0 }).ok, true);
  assert.equal(connect(p, { node: d.id, port: 0 }, { node: e.id, port: 0 }).ok, true);
  assert.equal(outType(p, { node: e.id, port: 0 }), 'ev');
  assert.equal(connect(p, { node: e.id, port: 0 }, { node: o.id, port: 0 }).ok, true);
  assert.equal(connect(p, { node: e.id, port: 0 }, { node: d.id, port: 0 }).ok, false); // 輪
  assert.equal(hasCycle(p), false);
  assert.deepEqual(validate(p).filter((i) => i.level === 'error'), []);
  const tg = toGraph(p);
  assert.deepEqual(tg.nodes.map((n) => n.input || n.inputs || null), [null, 'src1.video', 'dec1', ['enc1']]);
  assert.deepEqual(sinkOrigins(g), [{ sink: 'out', stream: 'O', from: 'program', compose: 'prod' }]);
});
t('pipeline API helpers: issues from problem details, status/meters split', () => {
  assert.deepEqual(issuesOf({ details: { issues: [{ code: 'POLICY_REJECTED', node: 'venc', message: 'm' }] } }), [{ code: 'POLICY_REJECTED', node: 'venc', message: 'm' }]);
  assert.equal(issuesOf({ code: 'INVALID_CONFIG', details: { reason: 'Invalid(..)' } })[0].message, 'Invalid(..)');
  const rt = runtimeOf({ status: { event: 'meters', node: 'mix', inputs: [], master: {} }, meters: {} });
  assert.equal(rt.status, null); assert.ok(rt.meters.mix);
  assert.equal(runtimeOf({ status: { event: 'status', compose: [] }, meters: null }).status.event, 'status');
  const subs = parseSubtitleMedia('#EXTM3U\n#EXT-X-MEDIA:TYPE=SUBTITLES,GROUP-ID="subs",NAME="English",LANGUAGE="en",DEFAULT=NO,AUTOSELECT=YES,URI="sub_en.m3u8"\n', 'http://h/s/1/x/live/id/master.m3u8');
  assert.deepEqual(subs, [{ language: 'en', name: 'English', uri: 'http://h/s/1/x/live/id/sub_en.m3u8', default: false }]);
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

# ---- 試験の素材と送り手（ブラウザの前に用意する）
# VP9＋Opus の 2 本（パイプラインの入力。node.sh の J と同じく mp4-to-flv ＋ rtmp_push）と、H.264＋AAC（WHEP のプレビュー用）
HAVE_VPX=0; [ -e "$CODEC_LIB_DIR/libvpx.so.12" ] && [ ${#CODEC_ARGS[@]} -gt 0 ] && HAVE_VPX=1
if [ $HAVE_VPX = 1 ]; then
  for n in 1 2; do
    "$FFMPEG" -v error -f lavfi -i "testsrc2=size=640x360:rate=30" -f lavfi -i "sine=frequency=$((300*n)):sample_rate=48000" -t 20 \
      -c:v libvpx-vp9 -deadline realtime -cpu-used 8 -g 30 -b:v 800k -c:a libopus "$W/vp9_$n.mp4"
    "$TARGET/debug/mp4-to-flv" "$W/vp9_$n.mp4" "$W/vp9_$n.flv"
  done
fi
"$FFMPEG" -v error -f lavfi -i testsrc2=size=640x360:rate=30 -f lavfi -i sine=frequency=500:sample_rate=48000 -t 10 \
  -c:v libx264 -profile:v baseline -g 30 -bf 0 -pix_fmt yuv420p -c:a aac -b:a 96k -f flv "$W/h264.flv"
mk() { api POST /streams -d "$2" > "$W/s_$1.json"; }
sid() { json "['id']" < "$W/s_$1.json"; }
skey() { json "['ingest']['stream_key']" < "$W/s_$1.json"; }
mk pv1 '{"name":"pv1","codec_profile":"royalty_free","hls":{"container":"fmp4"}}'
mk pv2 '{"name":"pv2","codec_profile":"royalty_free","hls":{"container":"fmp4"}}'
mk pout '{"name":"pout","codec_profile":"royalty_free","hls":{"container":"fmp4"}}'
mk whep264 '{"name":"whep264","hls":{"container":"fmp4"}}'
mk whipcam '{"name":"whipcam","hls":{"container":"fmp4"}}'
python3 - "$W" "$HAVE_VPX" <<'PY'
import json,sys
w=sys.argv[1]; s=lambda n: json.load(open(f"{w}/s_{n}.json"))
json.dump({"pv1":s("pv1")["id"],"pv2":s("pv2")["id"],"pout":s("pout")["id"],"whep264":s("whep264")["id"],
           "whipcam":s("whipcam")["id"],"whipkey":s("whipcam")["ingest"]["stream_key"],"vpx":sys.argv[2]=="1"},open(f"{w}/env.json","w"))
PY
# 送り手: H.264 は実時間でずっと、VP9 は速く送るので繰り返す（止めるまで）
( while [ ! -e "$W/stop-all" ]; do "$FFMPEG" -v error -re -stream_loop -1 -i "$W/h264.flv" -c copy -f flv "rtmp://127.0.0.1:$RTMP/live/$(skey whep264)" >>"$W/pub264.log" 2>&1 & P=$!
    while [ ! -e "$W/stop-all" ] && kill -0 $P 2>/dev/null; do sleep 0.5; done; kill $P 2>/dev/null; wait $P 2>/dev/null; done ) &
FEED_PIDS+=($!)
if [ $HAVE_VPX = 1 ]; then
  for n in 1 2; do
    ( k=$(skey pv$n); while [ ! -e "$W/stop-all" ]; do "$TARGET/debug/examples/rtmp_push" 127.0.0.1:$RTMP live "$k" "$W/vp9_$n.flv" >/dev/null 2>&1; sleep 2; done ) &
    FEED_PIDS+=($!)
  done
fi

cat > "$W/browser.cjs" <<'EOF'
// 管理画面をヘッドレスの Chrome で操作する。結果は {checks:[{name,ok,detail}]} を result.json に書く。
const fs = require('fs');
const path = require('path');
const { execSync } = require('child_process');
const [base, token, work] = process.argv.slice(2);
const env = JSON.parse(fs.readFileSync(path.join(work, 'env.json'), 'utf8'));
function findPw() {
  try { return require.resolve('playwright-core'); } catch { /* 探す */ }
  const out = execSync("find / -path '*/node_modules/playwright-core/package.json' -not -path '/proc/*' 2>/dev/null | head -1").toString().trim();
  if (!out) throw new Error('playwright-core not found');
  return path.dirname(out);
}
const { chromium } = require(findPw());
const checks = [];
const check = (name, ok, detail = '') => { checks.push({ name, ok: !!ok, detail: String(detail).slice(0, 500) }); };
const sleep = (ms) => new Promise((r) => setTimeout(r, ms));
const waitFile = async (f, ms) => { const end = Date.now() + ms; while (Date.now() < end) { if (fs.existsSync(f)) return true; await sleep(200); } return false; };
// 試験の中で意図して起こす失敗（[状態, URL]）。ブラウザはこれらも console に出すので除く
const EXPECTED_FAIL = [
  [/401/, /\/api\/v1\/capabilities$/], // 間違ったトークンでのログイン
  [/412/, /\/api\/v1\/recordings\/[^/?]+$/], // 古い ETag
  [/40[49]|503/, /\/api\/v1\/streams\/[^/]+\/snapshot(\?|$)/], // 今の画が無い・作れない
  [/404/, /\/s\/\d+\/[^/]+\/live\/[^/]+\/(index|master)\.m3u8/], // 送り手を止めた後のライブ・字幕の無い master
  [/409/, /\/api\/v1\/secrets\/[^/?]+$/], // 使われている秘密値の削除
  [/400/, /\/api\/v1\/inputs\/[^/?]+$/], // URL に資格情報を入れた入力の変更
  [/400|503/, /\/api\/v1\/pipelines\/[^/]+\/validate$/], // 方針に反するグラフの検証
  [/422/, /\/whep\/[^/?]+/], // まだ配信していない出力の WHEP（HLS に戻す）
];

(async () => {
  const exe = fs.existsSync('/usr/bin/google-chrome') ? '/usr/bin/google-chrome' : undefined;
  const browser = await chromium.launch({ executablePath: exe, headless: true, args: [
    '--autoplay-policy=no-user-gesture-required', '--use-fake-device-for-media-stream', '--use-fake-ui-for-media-stream'] });
  const ctx = await browser.newContext({ viewport: { width: 1360, height: 900 }, acceptDownloads: true, locale: 'ja-JP', timezoneId: 'Asia/Tokyo', permissions: ['camera', 'microphone'] });
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
    for (let i = 0; i < 3 && await page.locator('dialog[open]').count(); i++) await page.keyboard.press('Escape');
    try { await fn(); } catch (e) { check(name, false, e.message); await shot(`fail-${name.replace(/\W+/g, '_')}`); }
  };
  const clearToasts = () => page.evaluate(() => document.getElementById('toasts').replaceChildren());
  const toastText = (re, ms = 8000) => page.locator('.toast', { hasText: re }).first().waitFor({ timeout: ms });
  const apiGet = (p) => page.evaluate(async ([p, token]) => { const r = await fetch(`/api/v1${p}`, { headers: { Authorization: `Bearer ${token}` } }); return { status: r.status, body: await r.json().catch(() => null) }; }, [p, token]);
  const waitApi = async (p, pred, ms) => { const end = Date.now() + ms; let last; while (Date.now() < end) { last = await apiGet(p); if (pred(last.body)) return last.body; await sleep(500); } throw new Error(`timeout waiting ${p}: ${JSON.stringify(last && last.body).slice(0, 300)}`); };
  const videoAdvances = async (sel, ms) => {
    await page.waitForFunction((s) => { const v = document.querySelector(s); return v && v.currentTime > 1; }, sel, { timeout: ms });
    const t1 = await page.locator(sel).evaluate((v) => v.currentTime);
    await sleep(2000);
    const t2 = await page.locator(sel).evaluate((v) => v.currentTime);
    return { t1, t2, ok: t2 > t1 + 1 };
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
    await page.getByLabel('再生の方式').selectOption('hls');
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
    await page.getByRole('button', { name: '■ 停止' }).first().click();
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
    await page.goto(`${base}/console/#/recordings`);
    await page.locator('table a').first().click();
    await page.getByRole('heading', { name: '再生（VOD）' }).waitFor();
    await page.locator('.badge.ok', { hasText: '確定' }).first().waitFor({ timeout: 30000 });
    check('recording finalized (SSE refresh)', true);
    await page.getByRole('button', { name: '▶ 再生' }).click();
    await page.waitForFunction(() => { const v = document.querySelector('video[aria-label="録画の再生"]'); return v && v.currentTime > 2; }, null, { timeout: 20000 });
    const d = await page.locator('video[aria-label="録画の再生"]').evaluate((v) => ({ t: v.currentTime, d: v.duration }));
    check('MSE VOD plays with duration', d.d > 4 && isFinite(d.d), JSON.stringify(d));
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

  // ---------------------------------------------------------------- 秘密値・入力・出力・字幕
  const SECRET_VALUE = 'sv-' + Math.random().toString(36).slice(2);
  await step('secrets create', async () => {
    await page.goto(`${base}/console/#/secrets`);
    await page.getByLabel('名前', { exact: true }).fill('sinkkey');
    await page.getByLabel('値（表示・保存しません）').fill(SECRET_VALUE);
    await page.getByRole('button', { name: '作成' }).click();
    await page.getByRole('textbox', { name: 'sinkkey の参照' }).waitFor({ timeout: 5000 });
    const ref = await page.getByRole('textbox', { name: 'sinkkey の参照' }).inputValue();
    const leaked = await page.evaluate((v) => document.body.innerHTML.includes(v) || [...Object.values(localStorage), ...Object.values(sessionStorage)].some((x) => x.includes(v)), SECRET_VALUE);
    const valEmpty = (await page.getByLabel('値（表示・保存しません）').inputValue()) === '';
    check('secret created (write-only; value not in DOM/storage, field cleared)', ref === 'secret:sinkkey' && !leaked && valEmpty, `ref=${ref} leaked=${leaked}`);
    await shot('07-secrets');
  });

  await step('outputs and inputs CRUD', async () => {
    await page.goto(`${base}/console/#/streams/${env.whep264}`);
    await page.getByRole('heading', { name: 'whep264' }).waitFor();
    const outSec = page.locator('section', { has: page.getByRole('heading', { name: '出力（push・PaaS）' }) });
    await outSec.getByRole('button', { name: '＋ 出力を追加' }).click();
    let dlg = page.getByRole('dialog');
    await dlg.getByLabel('方式').selectOption('rtmp_push');
    await dlg.getByLabel('URL').fill('rtmp://127.0.0.1:9/app');
    await dlg.getByLabel('資格情報（秘密値）').selectOption('secret:sinkkey');
    await dlg.getByRole('button', { name: '追加' }).click();
    await outSec.getByRole('cell', { name: 'rtmp://127.0.0.1:9/app' }).waitFor({ timeout: 8000 });
    await outSec.getByRole('button', { name: '再読み込み' }).click();
    await outSec.locator('td .badge').first().waitFor({ timeout: 8000 });
    const st = await outSec.locator('td .badge').first().textContent();
    check('output created (rtmp_push + secret ref) with status', true, `status=${st}`);
    await outSec.getByRole('button', { name: '編集' }).click();
    dlg = page.getByRole('dialog');
    await dlg.getByLabel('URL').fill('rtmp://127.0.0.1:9/app2');
    await dlg.getByRole('button', { name: '保存' }).click();
    await outSec.getByRole('cell', { name: 'rtmp://127.0.0.1:9/app2' }).waitFor({ timeout: 8000 });
    await outSec.getByRole('button', { name: '無効にする' }).click();
    await outSec.locator('.badge', { hasText: '無効' }).waitFor({ timeout: 8000 });
    check('output edit (PATCH If-Match) and disable', true);
    // 使われている秘密値は消せない
    await page.goto(`${base}/console/#/secrets`);
    await clearToasts();
    await page.getByRole('button', { name: '秘密値 sinkkey を削除' }).click();
    await page.getByRole('dialog').getByRole('button', { name: '削除' }).click();
    await toastText(/消せません/);
    check('secret in use → 409 explained', true);
    await page.goto(`${base}/console/#/streams/${env.whep264}`);
    await outSec.getByRole('button', { name: /出力 rtmp_push を削除/ }).click();
    await page.getByRole('dialog').getByRole('button', { name: '削除' }).click();
    await outSec.getByText('出力はありません').waitFor({ timeout: 8000 });
    check('output delete', true);
    const inSec = page.locator('section', { has: page.getByRole('heading', { name: '入力（pull）' }) });
    await inSec.getByRole('button', { name: '＋ 入力を追加' }).click();
    dlg = page.getByRole('dialog');
    await dlg.getByLabel('方式').selectOption('hls_pull');
    await dlg.getByLabel('URL').fill('http://127.0.0.1:9/live/index.m3u8');
    await dlg.getByLabel('有効').uncheck();
    await dlg.getByRole('button', { name: '追加' }).click();
    await inSec.getByRole('cell', { name: 'http://127.0.0.1:9/live/index.m3u8' }).waitFor({ timeout: 8000 });
    // URL に資格情報を入れると拒否される
    await inSec.getByRole('button', { name: '編集' }).click();
    dlg = page.getByRole('dialog');
    await dlg.getByLabel('URL').fill('http://u:p@127.0.0.1:9/x.m3u8');
    await dlg.getByRole('button', { name: '保存' }).click();
    await dlg.locator('.alert.error').waitFor({ timeout: 5000 });
    const why = await dlg.locator('.alert.error').textContent();
    await dlg.getByRole('button', { name: 'やめる' }).click();
    check('input create, and credentials in URL rejected with reason', /資格情報/.test(why), why);
    await inSec.getByRole('button', { name: /入力 hls_pull を削除/ }).click();
    await page.getByRole('dialog').getByRole('button', { name: '削除' }).click();
    await inSec.getByText('入力はありません').waitFor({ timeout: 8000 });
    check('input delete', true);
    await page.goto(`${base}/console/#/secrets`);
    await page.getByRole('button', { name: '秘密値 sinkkey を削除' }).click();
    await page.getByRole('dialog').getByRole('button', { name: '削除' }).click();
    await page.getByText('秘密値はありません').waitFor({ timeout: 5000 });
    check('secret delete after unused', true);
  });

  await step('captions settings', async () => {
    await page.goto(`${base}/console/#/streams/${env.whep264}`);
    await page.getByRole('heading', { name: 'whep264' }).waitFor();
    await page.getByRole('button', { name: '字幕の設定を編集' }).click();
    const dlg = page.getByRole('dialog');
    await dlg.getByText('外部のサービスへ送ります。').waitFor({ timeout: 5000 });
    const notice = await dlg.getByText('外部のサービスへ送ります。').isVisible();
    await dlg.getByLabel('字幕を作る（音声認識）').check();
    await dlg.getByLabel('提供者', { exact: true }).selectOption('whisper_cpp');
    await dlg.getByLabel('送り先の URL').first().fill('http://127.0.0.1:9/inference');
    await dlg.getByLabel('話している言語（ISO 639-1）').fill('ja');
    await dlg.getByLabel('翻訳の提供者').selectOption('openai');
    await dlg.getByLabel('送り先の URL').nth(1).fill('http://127.0.0.1:9/v1');
    await dlg.getByLabel(/翻訳先の言語/).fill('en, ko');
    await dlg.getByRole('button', { name: '保存' }).click();
    await page.getByText('字幕の設定を保存しました').waitFor({ timeout: 8000 });
    const caps = (await apiGet(`/streams/${env.whep264}`)).body.captions;
    check('captions saved (PATCH stream.captions) with external-service notice', notice && caps && caps.asr_provider === 'whisper_cpp' && caps.translate_provider === 'openai' && caps.targets.join(',') === 'en,ko', JSON.stringify(caps));
    await page.getByText('有効です: この配信の音声は上の送り先へ送られます').waitFor();
    // 再生 URL（WHEP・RTSP）と字幕トラック
    await page.getByRole('button', { name: '再生 URL を発行' }).click();
    const whep = await page.getByRole('textbox', { name: 'WHEP の URL' }).inputValue();
    const rtsp = await page.getByRole('textbox', { name: 'RTSP の URL' }).inputValue();
    await page.locator('[data-subtitles], [aria-label$="の字幕"]').first().waitFor({ timeout: 10000 });
    const subs = await page.locator('section', { has: page.getByRole('heading', { name: '再生 URL' }) }).textContent();
    check('playback URLs: copyable WHEP and RTSP; subtitle tracks probed via master.m3u8', /\/whep\/.+exp=.+sig=/.test(whep) && /^rtsp:\/\/.+sig=/.test(rtsp), `whep=${whep.slice(0, 60)} rtsp=${rtsp.slice(0, 60)} subs=${/字幕トラック(はまだ|はありません)/.test(subs) ? 'none yet' : 'listed'}`);
    await shot('08-captions');
    await page.getByRole('button', { name: '字幕の設定を編集' }).click();
    await page.getByRole('dialog').getByLabel('字幕を作る（音声認識）').uncheck();
    await page.getByRole('dialog').getByRole('button', { name: '保存' }).click();
    await page.getByText('字幕を止めました').waitFor({ timeout: 8000 });
    check('captions disabled (captions: null)', (await apiGet(`/streams/${env.whep264}`)).body.captions === null);
  });

  // ---------------------------------------------------------------- WHEP のプレビュー（H.264 の RTMP）
  await step('whep preview', async () => {
    await page.goto(`${base}/console/#/streams/${env.whep264}`);
    await page.locator('.badge.live').first().waitFor({ timeout: 20000 });
    await page.getByLabel('再生の方式').selectOption('webrtc');
    await page.getByRole('button', { name: '▶ 再生' }).click();
    const v = await videoAdvances('video[aria-label="ライブのプレビュー"]', 20000);
    await sleep(1500);
    const status = await page.locator('.player-status').first().textContent();
    const m = /復号 (\d+)/.exec(status || '');
    const decoded = m ? Number(m[1]) : 0;
    const srcObj = await page.locator('video[aria-label="ライブのプレビュー"]').evaluate((x) => !!x.srcObject);
    check('WHEP preview plays H.264 RTMP stream (currentTime advances, getStats framesDecoded>0)', v.ok && decoded > 0 && srcObj && /WebRTC/.test(status), `t1=${v.t1.toFixed(2)} t2=${v.t2.toFixed(2)} ${status}`);
    await shot('09-whep');
    await page.getByRole('button', { name: '■ 停止' }).first().click();
  });

  // ---------------------------------------------------------------- パイプライン（VP9 → AV1）
  let pipelineId = null;
  await step('pipeline editor', async () => {
    if (!env.vpx) { check('pipeline editor (skipped: no libvpx / codec worker)', true); return; }
    await page.goto(`${base}/console/#/pipelines`);
    await page.getByRole('heading', { name: 'パイプライン', exact: true, level: 1 }).waitFor();
    const graph = {
      version: 1, options: { clock: 'realtime' }, nodes: [
        { type: 'source', id: 'cam_a', stream: env.pv1, video: { codec: 'vp9' }, audio: { codec: 'opus' } },
        { type: 'source', id: 'cam_b', stream: env.pv2, video: { codec: 'vp9' } },
        { type: 'decode', id: 'dec_a', input: 'cam_a.video' }, { type: 'decode', id: 'dec_b', input: 'cam_b.video' }, { type: 'decode', id: 'adec_a', input: 'cam_a.audio' },
        { type: 'compose', id: 'prod', inputs: { a: 'dec_a', b: 'dec_b' }, canvas: { width: 640, height: 360, fps_num: 30 },
          scenes: { pinp: { layers: [{ source: 'b', x: 0, y: 0, width: 640, height: 360 }, { source: 'a', x: 440, y: 250, width: 192, height: 108, z: 1 }] },
            cam_a: { layers: [{ source: 'a', x: 0, y: 0, width: 640, height: 360 }] } },
          program: 'pinp', preview: 'cam_a', transition: { kind: 'fade', duration_ms: 300 } },
        { type: 'audio_mix', id: 'mix', inputs: { mic_a: { from: 'adec_a' } } },
        { type: 'encode', id: 'venc', input: 'prod', codec: 'av1', bitrate_kbps: 800, gop: 30 },
        { type: 'encode', id: 'aenc', input: 'mix', codec: 'opus', bitrate_kbps: 96 },
        { type: 'sink', id: 'out', inputs: ['venc', 'aenc'], stream: env.pout },
      ],
    };
    await page.getByRole('button', { name: 'JSON', exact: true }).click();
    await page.getByLabel('パイプラインの定義（JSON）').fill(JSON.stringify({ name: 'pinp-browser', graph }));
    await page.getByRole('button', { name: '取り込む' }).click();
    await page.locator('[data-node="venc"]').waitFor();
    const nodes = await page.locator('.pl-node').count();
    const edges = await page.locator('.pl-edge').count();
    check('pipeline graph imported into editor (nodes and edges drawn)', nodes === 10 && edges === 10, `${nodes} nodes ${edges} edges`);
    await clearToasts();
    await page.getByRole('button', { name: '保存', exact: true }).click();
    await toastText(/ノードに保存しました/);
    const list = (await apiGet('/pipelines')).body.items;
    const created = list.find((x) => x.name === 'pinp-browser');
    pipelineId = created && created.id;
    check('pipeline saved (POST /pipelines)', !!created && created.desired_state === 'stopped' && JSON.stringify(created.graph.nodes.map((n) => n.id)) === JSON.stringify(graph.nodes.map((n) => n.id)), created ? created.id : 'none');
    await clearToasts();
    await page.getByRole('button', { name: '検証', exact: true }).click();
    await toastText(/ノードの検証に通りました/);
    await page.locator('section', { has: page.getByRole('heading', { name: '計画', exact: true }) }).getByRole('cell', { name: 'venc' }).first().waitFor({ timeout: 5000 });
    const planText = await page.locator('section', { has: page.getByRole('heading', { name: '計画', exact: true }) }).textContent();
    check('VP9/AV1 graph validates on node → plan shown', /av1/.test(planText) && /vp9/.test(planText), planText.slice(0, 200));
    // 方針に反する変更（royalty_free の stream で H.264）→ 問題の一覧
    await page.locator('[data-node="venc"]').click();
    await page.getByLabel('コーデック（必須）').selectOption('h264');
    await clearToasts();
    await page.getByRole('button', { name: '検証', exact: true }).click();
    await page.locator('ul[data-remote] li').first().waitFor({ timeout: 8000 });
    const issues = await page.locator('ul[data-remote]').textContent();
    check('policy violation → node issue list from problem details', /venc/.test(issues) || /h264|H\.264|POLICY|CAPACITY/i.test(issues), issues.slice(0, 200));
    await page.getByLabel('コーデック（必須）').selectOption('av1');
    await page.getByLabel('ビットレート（kbps）（必須）').fill('700');
    await page.getByLabel('ビットレート（kbps）（必須）').press('Tab');
    await clearToasts();
    await page.getByRole('button', { name: '保存', exact: true }).click();
    await toastText(/ノードに保存しました/);
    const after = (await apiGet(`/pipelines/${pipelineId}`)).body;
    check('pipeline edit saved (PATCH graph with If-Match → new generation)', after.generation === 2 && after.graph.nodes.find((n) => n.id === 'venc').bitrate_kbps === 700, `gen=${after.generation}`);
    await clearToasts();
    await page.getByRole('button', { name: '▶ 開始' }).click();
    await toastText(/開始しました/);
    await page.locator('.toolbar .badge', { hasText: 'running' }).waitFor({ timeout: 30000 });
    check('pipeline start (PATCH desired_state) → observed running', true);
    await waitApi(`/streams/${env.pout}`, (b) => b && b.observed_state === 'live', 60000);
    check('pipeline output stream turns live', true);
    await shot('10-pipeline');
  });

  // ---------------------------------------------------------------- スイッチャー
  await step('switcher', async () => {
    if (!pipelineId) { check('switcher (skipped: no pipeline)', !env.vpx); return; }
    await page.goto(`${base}/console/#/production?pipeline=${pipelineId}`);
    await page.locator('[data-scene="pinp"]').waitFor({ timeout: 10000 });
    await page.locator('[data-program="pinp"]').waitFor({ timeout: 10000 });
    await page.locator('[data-scene="cam_a"]').click();
    await page.locator('[data-preview="cam_a"]').waitFor({ timeout: 5000 });
    await page.getByRole('button', { name: 'CUT', exact: true }).click();
    await page.locator('[data-program="cam_a"]').waitFor({ timeout: 5000 });
    await sleep(2500);
    const stillA = await page.locator('[data-program="cam_a"]').count();
    // ノードの status（届いていれば）で確かめる
    let confirmed = 'status not observed';
    const end = Date.now() + 6000;
    while (Date.now() < end) {
      const b = (await apiGet(`/pipelines/${pipelineId}`)).body;
      const st = b && b.status && b.status.event === 'status' ? b.status : null;
      const cs = st && st.compose.find((c) => c.node === 'prod');
      if (cs) { confirmed = `node program=${cs.program} preview=${cs.preview}`; if (cs.program === 'cam_a') break; }
      await sleep(150);
    }
    check('switcher: set_preview + CUT (take) round-trip', stillA === 1 && !/program=pinp/.test(confirmed), confirmed);
    await page.getByRole('button', { name: 'FADE', exact: true }).click();
    await page.locator('[data-program="pinp"]').waitFor({ timeout: 5000 });
    await page.locator('[data-scene="cam_a"]').dblclick();
    await page.locator('[data-program="cam_a"]').waitFor({ timeout: 5000 });
    check('switcher: FADE and double-click switch_program', true);
    // 音声: フェーダー → set_audio、メーター（届いていれば）でノードの値を確かめる
    const fader = page.getByLabel('mic_a の音量（dB）');
    await fader.evaluate((el) => { el.value = '-12'; el.dispatchEvent(new Event('input', { bubbles: true })); });
    let gainSeen = null;
    const end2 = Date.now() + 8000;
    while (Date.now() < end2) {
      gainSeen = await page.locator('[data-strip="mix/mic_a"] [data-server-gain]').getAttribute('data-server-gain');
      if (gainSeen === '-12') break;
      await sleep(300);
    }
    const meterNow = await page.locator('[data-strip="mix/mic_a"] .meter').getAttribute('aria-valuenow');
    check('switcher: fader → set_audio (node-reported gain via meters)', gainSeen === '-12' || gainSeen === null, `server gain=${gainSeen} meter=${meterNow}`);
    const mute = page.locator('[data-strip="mix/mic_a"] button', { hasText: 'ミュート' });
    await mute.click();
    await page.waitForFunction(() => document.querySelector('[data-strip="mix/mic_a"] button[aria-pressed="true"]'), null, { timeout: 5000 });
    check('switcher: mute toggle', true);
    // プログラムのモニター（出力の stream を WHEP で）
    const pv = 'video[aria-label="プログラムのモニター"]';
    const playing = await page.waitForFunction((s) => { const v = document.querySelector(s); return v && v.currentTime > 1; }, pv, { timeout: 8000 }).then(() => true).catch(() => false);
    if (!playing) {
      await page.locator('.monitor.program').getByRole('button', { name: '▶ 再生' }).click();
    }
    const v = await videoAdvances(pv, 20000);
    const mstatus = await page.locator('.monitor.program .player-status').textContent();
    check('switcher: program monitor plays the sink stream (WHEP, AV1)', v.ok && /WebRTC/.test(mstatus), `${mstatus} t1=${v.t1.toFixed(1)} t2=${v.t2.toFixed(1)}`);
    await shot('11-switcher');
    // 止める
    await page.goto(`${base}/console/#/pipelines?id=${pipelineId}`);
    await page.getByRole('button', { name: '■ 停止' }).click();
    await page.locator('.toolbar .badge', { hasText: 'stopped' }).waitFor({ timeout: 20000 });
    check('pipeline stop (PATCH desired_state)', true);
  });

  // ---------------------------------------------------------------- このブラウザから送る（WHIP）
  await step('whip send', async () => {
    await page.goto(`${base}/console/#/send?stream=${env.whipcam}`);
    await page.getByRole('heading', { name: 'このブラウザから送る' }).waitFor();
    await page.getByLabel('ストリームキー（保存しません）').fill(env.whipkey);
    await page.getByLabel('大きさ').selectOption('640x360');
    await page.getByRole('button', { name: '● 送信を開始' }).click();
    await page.locator('.player-status[data-state="live"]').waitFor({ timeout: 20000 });
    await page.waitForFunction(() => Number((document.querySelector('[data-stats]') || {}).dataset?.frames || 0) > 30, null, { timeout: 15000 });
    const frames = await page.locator('[data-stats]').getAttribute('data-frames');
    const s = await waitApi(`/streams/${env.whipcam}`, (b) => b && b.observed_state === 'live', 20000);
    const keyGone = (await page.getByLabel('ストリームキー（保存しません）').inputValue()) === '';
    const stored = await page.evaluate((k) => [...Object.values(localStorage), ...Object.values(sessionStorage), location.href].some((x) => x.includes(k)), env.whipkey);
    check('WHIP send from fake camera → stream live (key not stored)', s.live && s.live.transport === 'webrtc' && keyGone && !stored, `frames=${frames} transport=${s.live && s.live.transport} tracks=${JSON.stringify(s.live && s.live.tracks).slice(0, 120)}`);
    await shot('12-whip');
    await page.getByRole('button', { name: '■ 止める' }).click();
    await page.locator('.player-status[data-state="idle"]').waitFor({ timeout: 5000 });
    check('WHIP stop (DELETE session)', true);
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

docker run --rm --name senca-console-browser-$$ --network host --ipc=host -v "$W:$W" "$IMG" bash -c \
  "node '$W/browser.cjs' '$BASE' '$TOKEN' '$W'; chown -R $(id -u):$(id -g) '$W'" > "$W/browser.log" 2>&1 || true
touch "$W/stop-all"
wait "$PUB_PID" 2>/dev/null || true; PUB_PID=
if [ -s "$W/result.json" ]; then
  while IFS=$'\t' read -r ok name detail; do
    if [ "$ok" = True ]; then pass "D $name${detail:+ ($detail)}"; else fail "D $name${detail:+ — $detail}"; fi
  done < <(python3 -c 'import json,sys
for c in json.load(open(sys.argv[1]))["checks"]: print("%s\t%s\t%s" % (c["ok"], c["name"], c["detail"].replace("\n"," ").replace("\t"," ")[:200]))' "$W/result.json")
  SHOTS=${SHOTS:-$HOME/.cache/senca-console-shots}
  mkdir -p "$SHOTS"
  cp "$W"/*.png "$SHOTS/" 2>/dev/null && echo "screenshots: $SHOTS"
else
  fail "D browser produced no result"; tail -30 "$W/browser.log"
fi
grep -q "sk_" "$W/node.log" && fail "D stream key leaked to node log" || pass "D no stream key in node log"
summary
