#!/usr/bin/env bash
# 部屋のカスケード（ADR-0015）と Webhook を 3 台のクラスタで: 同じ部屋の参加者が別々のノードにつなぎ（edge）、
# Chrome（Docker の ov-chrome ＋ playwright-core）で互いの映像を復号し、DataChannel の message がノードをまたいで届く。
# 部屋を置いたノード（home）を落とすと、残りの参加者はそのまま、落ちたノードの参加者は別のノードへ入り直し、
# 全員がまた互いを復号する。認証 Webhook（許可・拒否・役割の上書き・部屋ごとの URL）とイベント Webhook
# （署名、500 での送り直し）は Python の受け手（webhook_mock.py）で確かめる。
# ほかに: SSE のイベントの id がどのノードでも同じで、別のノードへ Last-Event-ID でつなぎ直すと続きが届くこと、edge の参加者を
# どのノードの REST からでも変更・切断できること、メディアパイプラインが edge の参加者も書き出すこと、配信を受けていない
# ノードでの録画の開始が 409 OWNERSHIP_LOST になることを確かめる。
#
# 使い方: tools/interop/cluster_rooms.sh（KEEP=1 で作業場所を残す。SENCA_PORT_MIN/MAX で番号の範囲を狭める）
set -euo pipefail
source "$(dirname "$0")/lib.sh"
IMG=${BROWSER_IMAGE:-ov-chrome:latest}
if ! docker image inspect "$IMG" >/dev/null 2>&1; then echo "SKIP: Docker image $IMG がない"; exit 0; fi
[ -n "${NODE_BIN_PREBUILT:-}" ] || (cd "$ROOT" && cargo build -q -p senca-node)
mkdir -p "$HOME/.cache"
W=$(mktemp -d -p "$HOME/.cache" senca-cluster-rooms.XXXXXX)
declare -A PIDS
EXTRA=(); C="senca-cluster-rooms-$$"
cleanup() {
  for p in "${PIDS[@]}" "${EXTRA[@]}"; do [ -n "$p" ] && kill "$p" 2>/dev/null || true; done
  docker stop -t 2 "$C" >/dev/null 2>&1 || true
  wait 2>/dev/null || true
  if [ -n "${KEEP:-}" ]; then echo "kept $W"; else rm -rf "$W"; fi
}
trap cleanup EXIT
IP=${HOST_IP:-$(hostname -I | awk '{print $1}')}
echo "0123456789abcdef-cluster-secret" > "$W/secret"
echo "webhook-secret-0123456789abcdef" > "$W/hook-secret"
declare -A HTTP RAFT RELAY UDPP
for i in 1 2 3; do HTTP[$i]=$(port); RAFT[$i]=$(port); RELAY[$i]=$(port); UDPP[$i]=$(port); done
WP=$(port); PP=$(port)
python3 "$ROOT/tools/interop/webhook_mock.py" "$WP" "$W/hook-secret" >"$W/mock.log" 2>&1 & EXTRA+=("$!")
for i in 1 2 3; do
  cat > "$W/c$i.json" <<J
{"node_id": $i, "listen": "127.0.0.1:${RAFT[$i]}", "relay_listen": "127.0.0.1:${RELAY[$i]}", "secret_file": "$W/secret",
 "peers": [{"id":1,"addr":"127.0.0.1:${RAFT[1]}","relay":"127.0.0.1:${RELAY[1]}","api":"http://127.0.0.1:${HTTP[1]}"},
           {"id":2,"addr":"127.0.0.1:${RAFT[2]}","relay":"127.0.0.1:${RELAY[2]}","api":"http://127.0.0.1:${HTTP[2]}"},
           {"id":3,"addr":"127.0.0.1:${RAFT[3]}","relay":"127.0.0.1:${RELAY[3]}","api":"http://127.0.0.1:${HTTP[3]}"}]}
J
done
start() {
  local i=$1
  "$NODE_BIN" --data-dir "$W/d$i" --http 127.0.0.1:${HTTP[$i]} --rtmp off --public-http-url http://127.0.0.1:${HTTP[$i]} \
    --cluster-config "$W/c$i.json" --node-name n$i --egress-allow 127.0.0.0/8 --webrtc-udp "$IP:${UDPP[$i]}" \
    --rooms-auth-webhook "http://127.0.0.1:$WP/auth" --rooms-event-webhook "http://127.0.0.1:$WP/events" \
    --rooms-webhook-secret-file "$W/hook-secret" >>"$W/n$i.log" 2>&1 &
  PIDS[$i]=$!
}
ready() { for _ in $(seq 100); do curl -sf http://127.0.0.1:${HTTP[$1]}/readyz >/dev/null && return 0; sleep 0.2; done; return 1; }
for i in 1 2 3; do start $i; done
for i in 1 2 3; do ready $i || { fail "node $i not ready"; tail -5 "$W/n$i.log"; summary; exit 1; }; done
for _ in $(seq 50); do [ -s "$W/d1/control/bootstrap-admin-token" ] && break; sleep 0.2; done
TOKEN=$(cat "$W/d1/control/bootstrap-admin-token")
pass "3 nodes ready with webhooks configured"
api() { local n=$1 m=$2 p=$3; shift 3; curl -s -X "$m" -H "Authorization: Bearer $TOKEN" -H 'Content-Type: application/json' "http://127.0.0.1:${HTTP[$n]}/api/v1$p" "$@"; }
code() { local n=$1 m=$2 p=$3; shift 3; curl -s -o /dev/null -w '%{http_code}' -X "$m" -H "Authorization: Bearer $TOKEN" -H 'Content-Type: application/json' "http://127.0.0.1:${HTTP[$n]}/api/v1$p" "$@"; }
mock() { curl -s "http://127.0.0.1:$WP/stats"; }
mj() { mock | python3 -c "import json,sys; d=json.load(sys.stdin); print(eval(sys.argv[1]))" "$1"; }
probe() { python3 "$ROOT/tools/interop/ws_probe.py" "ws://127.0.0.1:${HTTP[$1]}/signaling" "$2" "$3"; }
QN=1
placed() { api $QN GET "/rooms/$1" | python3 -c "import json,sys; d=json.load(sys.stdin)['placement_status']; print(d.get('node_name') or d.get('state'))"; }

# ---- 部屋: カスケードの部屋・カスケードでない部屋・部屋ごとの認証 Webhook ----
c=$(code 1 POST /rooms -d '{"id":"conf","settings":{"cascade":true,"max_participants":10,"video_codec":"vp8"}}')
[ "$c" = 201 ] && pass "cascade room created" || fail "create conf $c"
c=$(code 1 POST /rooms -d '{"id":"plain","settings":{"max_participants":10}}'); [ "$c" = 201 ] || fail "create plain $c"
c=$(code 1 POST /rooms -d "{\"id\":\"locked\",\"settings\":{\"auth_webhook_url\":\"http://127.0.0.1:$WP/auth-deny\"}}"); [ "$c" = 201 ] || fail "create locked $c"
c=$(code 1 POST /rooms -d '{"id":"bad","settings":{"auth_webhook_url":"ftp://x/"}}'); [ "$c" = 400 ] && pass "invalid auth_webhook_url 400" || fail "bad url $c"
for r in conf plain locked; do for _ in $(seq 60); do case "$(placed $r)" in n1|n2|n3) break;; esac; sleep 0.25; done; done
HOME_N=$(placed conf); H=${HOME_N#n}
case "$H" in 1|2|3) pass "conf hosted on $HOME_N";; *) fail "conf placement $HOME_N"; summary; exit 1;; esac
E=$(( H % 3 + 1 ))   # home ではないノード
tok() { api 1 POST "/rooms/$1/tokens" -d "{\"role\":\"$2\",\"client_id\":\"$3\"}" | json "['token']"; }
# 認証 Webhook: 許可（カスケードの部屋に home でないノードからも入れる: redirect しない）
r=$(probe $E conf "$(tok conf recvonly probe)")
echo "$r" | grep -q '"type": *"connected"' && pass "cascade: joined conf on non-home n$E (no redirect)" || fail "cascade join on n$E: $r"
grep -q "rooms: room conf joined here as an edge" "$W/n$E.log" && pass "cascade: n$E hosts an edge of conf" || fail "no edge log on n$E"
# 役割の上書き・拒否
r=$(probe $E conf "$(tok conf sendrecv dave)")
echo "$r" | grep -q '"role": *"sendonly"' && pass "auth webhook: role overridden to sendonly" || fail "role override: $r"
r=$(probe $H conf "$(tok conf sendrecv mallory)")
echo "$r" | grep -q 'banned by the test webhook' && pass "auth webhook: mallory denied with the webhook's reason" || fail "deny: $r"
# 部屋ごとの URL（ノードの URL より優先）
LH=$(placed locked); r=$(probe "${LH#n}" locked "$(tok locked recvonly alice)")
echo "$r" | grep -q 'room webhook says no' && pass "auth webhook: the room's auth_webhook_url wins over the node's" || fail "room webhook: $r"
# カスケードでない部屋は今まで通り案内する
PH=$(placed plain); PE=$(( ${PH#n} % 3 + 1 ))
r=$(probe $PE plain "$(tok plain recvonly probe)")
echo "$r" | grep -q '"type": *"redirect"' && pass "non-cascade room still redirects to its home" || fail "plain redirect: $r"
[ "$(mj "d['bad_signature']")" = 0 ] && [ "$(mj "d['auth']")" -ge 3 ] && pass "auth webhook requests are signed (HMAC verified by the receiver)" || fail "auth stats $(mock)"

# ---- SSE: イベントの id はどのノードでも同じ。別のノードへつなぎ直しても Last-Event-ID の続きから ----
sse() { local n=$1 last=$2 secs=$3
  if [ -n "$last" ]; then curl -sN --max-time "$secs" -H "Authorization: Bearer $TOKEN" -H "Last-Event-ID: $last" "http://127.0.0.1:${HTTP[$n]}/api/v1/events" || true
  else curl -sN --max-time "$secs" -H "Authorization: Bearer $TOKEN" "http://127.0.0.1:${HTTP[$n]}/api/v1/events" || true; fi; }
# イベントの種類と資源 ID → id（空なら無い）
evid() { python3 - "$1" "$2" "$3" <<'PY'
import json, sys
f, ty, rid = sys.argv[1:4]
for blk in open(f).read().split("\n\n"):
    kv = {}
    for line in blk.splitlines():
        k, _, v = line.partition(": ")
        kv[k] = v
    if kv.get("event") == ty and json.loads(kv.get("data") or "{}").get("resource_id") == rid:
        print(kv.get("id", "")); break
PY
}
sse 1 "" 4 > "$W/sse1.txt" & S1=$!; sse 2 "" 4 > "$W/sse2.txt" & S2=$!; sleep 0.6
api 3 POST /rooms -d '{"id":"sse-a","settings":{"max_participants":2}}' >/dev/null
wait $S1 $S2
I1=$(evid "$W/sse1.txt" room.updated sse-a); I2=$(evid "$W/sse2.txt" room.updated sse-a)
[ -n "$I1" ] && [ "$I1" = "$I2" ] && pass "SSE: the same event has the same id on every node ($I1)" || fail "SSE ids n1='$I1' n2='$I2'"
for r in sse-b sse-c; do api 1 POST /rooms -d "{\"id\":\"$r\",\"settings\":{\"max_participants\":2}}" >/dev/null; done
sse 3 "$I1" 3 > "$W/sse3.txt"
[ -n "$(evid "$W/sse3.txt" room.updated sse-b)" ] && [ -n "$(evid "$W/sse3.txt" room.updated sse-c)" ] && ! grep -q '^event: resync' "$W/sse3.txt" \
  && pass "SSE: reconnecting to another node with Last-Event-ID $I1 replays the missed events" || fail "SSE replay on n3: $(head -c 600 "$W/sse3.txt")"
for r in sse-a sse-b sse-c; do code 1 DELETE "/rooms/$r" >/dev/null; done

# ---- edge の参加者を、どのノードの REST からでも変更・切断できる（参加者のいるノードへ送る）----
python3 "$ROOT/tools/interop/ws_probe.py" "ws://127.0.0.1:${HTTP[$E]}/signaling" conf "$(tok conf recvonly holder)" 25 > "$W/hold.txt" & HOLD=$!
for _ in $(seq 50); do grep -qs '"type": *"connected"' "$W/hold.txt" && break; sleep 0.2; done
HCID=$(head -1 "$W/hold.txt" | json "['connection_id']" 2>/dev/null || true)
X=$(( 6 - H - E ))   # home でも edge でもないノード
c=$(api $X PATCH "/rooms/conf/participants/$HCID" -d '{"metadata":{"badge":"speaker"}}' -o "$W/upd.json" -w '%{http_code}')
[ "$c" = 200 ] && [ "$(json "['metadata']['badge']" < "$W/upd.json" 2>/dev/null)" = speaker ] \
  && pass "PATCH participant on edge n$E through n$X (→ home → edge) is 200" || fail "patch edge participant via n$X: $c $(cat "$W/upd.json")"
ok=0; for _ in $(seq 30); do b=$(api $H GET "/rooms/conf/participants/$HCID" | json "['metadata'].get('badge')" 2>/dev/null || true); [ "$b" = speaker ] && { ok=1; break; }; sleep 0.2; done
[ $ok = 1 ] && pass "the home sees the edge participant's new metadata" || fail "home metadata $(api $H GET "/rooms/conf/participants/$HCID")"
c=$(code $H DELETE "/rooms/conf/participants/$HCID")
for _ in $(seq 25); do grep -q '"type": *"close"\|"disconnected"' "$W/hold.txt" && break; sleep 0.2; done
[ "$c" = 204 ] && grep -q '"type": *"close"' "$W/hold.txt" && pass "DELETE participant on edge n$E through the home disconnects it" || fail "disconnect edge participant: $c $(tail -3 "$W/hold.txt")"
kill $HOLD 2>/dev/null || true
c=$(code $X DELETE "/rooms/conf/participants/$HCID"); [ "$c" = 404 ] && pass "the disconnected participant is gone (404)" || fail "after disconnect $c"

# ---- Chrome: 3 人が別々のノードに（1 人は home、2 人は edge）----
python3 - "$W/www/index.html" <<'PY'
import os, sys
os.makedirs(os.path.dirname(sys.argv[1]), exist_ok=True)
open(sys.argv[1], "w").write(r"""<!doctype html><html><head><meta charset="utf-8"><title>cascade</title></head>
<body><pre id="log"></pre><div id="videos"></div><script>
const q = new URLSearchParams(location.search);
const name = q.get('name'), room = q.get('room'), token = q.get('token');
const others = (q.get('others') || '').split(',').filter(Boolean);
const R = { name, p1: null, p2: null, errors: [], rejoins: 0, urls: [] };
const log = (...a) => { const s = a.map(x => typeof x === 'string' ? x : JSON.stringify(x)).join(' '); document.getElementById('log').textContent += s + '\n'; console.log(name + ': ' + s); };
const sleep = (ms) => new Promise(r => setTimeout(r, ms));
let stream, S;
function session(url) {
  const s = { url, slots: {}, recv: { audio: 0, video: 0 }, closed: false, pongs: new Set(), offering: false, pendingReneg: null, redirect: null };
  R.urls.push(url);
  const send = (m) => { if (s.ws.readyState === 1) s.ws.send(JSON.stringify(m)); };
  const gathered = () => new Promise((res) => {
    if (s.pc.iceGatheringState === 'complete') return res();
    const t = setTimeout(res, 2000);
    s.pc.addEventListener('icegatheringstatechange', () => { if (s.pc.iceGatheringState === 'complete') { clearTimeout(t); res(); } });
  });
  async function offer(want) {
    for (const kind of ['audio', 'video']) {
      while (want && s.recv[kind] < want[kind]) {
        const tr = s.pc.addTransceiver(kind, { direction: 'recvonly' });
        if (kind === 'video') { try { tr.setCodecPreferences(RTCRtpReceiver.getCapabilities('video').codecs.filter(c => /vp8|rtx/i.test(c.mimeType))); } catch (e) {} }
        s.recv[kind]++;
      }
    }
    s.offering = true;
    await s.pc.setLocalDescription(await s.pc.createOffer());
    await gathered();
    send({ type: 'offer', sdp: s.pc.localDescription.sdp });
  }
  async function handle(m) {
    switch (m.type) {
      case 'connected': log('connected', m.connection_id, 'participants', m.participants.length); await offer(m.recv_slots); break;
      case 'answer':
        await s.pc.setRemoteDescription({ type: 'answer', sdp: m.sdp });
        for (const x of m.slots) s.slots[x.mid] = x;
        s.offering = false;
        if (s.pendingReneg) { const w = s.pendingReneg; s.pendingReneg = null; await offer(w); }
        break;
      case 'slots': for (const x of m.slots) s.slots[x.mid] = x; break;
      case 'renegotiate': if (s.offering) s.pendingReneg = m; else await offer(m); break;
      case 'ping': send({ type: 'pong' }); break;
      case 'redirect': s.redirect = m.location; break;
      case 'error': R.errors.push(m); log('error', m); break;
    }
  }
  s.pc = new RTCPeerConnection({ bundlePolicy: 'max-bundle' });
  s.chat = s.pc.createDataChannel('#chat');
  s.chat.onmessage = (e) => {
    const [kind, from, to] = String(e.data).split(':');
    if (kind === 'ping' && from !== name && s.chat.readyState === 'open') s.chat.send(`pong:${from}:${name}`);
    if (kind === 'pong' && from === name) s.pongs.add(to);
  };
  s.pc.ontrack = (e) => {
    if (e.track.kind !== 'video') return;
    const v = document.createElement('video'); v.autoplay = true; v.muted = true; v.playsInline = true;
    v.srcObject = new MediaStream([e.track]); document.getElementById('videos').appendChild(v);
  };
  const vtr = s.pc.addTransceiver(stream.getVideoTracks()[0], { direction: 'sendonly' });
  try { vtr.setCodecPreferences(RTCRtpSender.getCapabilities('video').codecs.filter(c => /vp8|rtx/i.test(c.mimeType))); } catch (e) {}
  s.pc.addTransceiver(stream.getAudioTracks()[0], { direction: 'sendonly' });
  s.ws = new WebSocket(url, 'senca.rooms.v1');
  let chain = Promise.resolve();
  s.ws.onmessage = (e) => { const m = JSON.parse(e.data); chain = chain.then(() => handle(m)).catch((err) => { R.errors.push(String(err)); log('handler', String(err)); }); };
  s.ws.onclose = (e) => { s.closed = true; log('ws closed', e.code, e.reason); };
  s.ws.onopen = () => send({ type: 'connect', version: 1, room_id: room, role: 'sendrecv', token, data_channels: [{ label: '#chat' }], video: { codec: 'vp8' }, sdk: { name: 'cascade-page', version: '1' } });
  return s;
}
// mid ごとの復号したフレーム数と、その枠の送り手
async function frames() {
  const out = [];
  (await S.pc.getStats()).forEach((r) => {
    if (r.type === 'inbound-rtp' && r.kind === 'video') {
      const x = S.slots[r.mid];
      out.push({ mid: r.mid, client: x && x.client_id, decoded: r.framesDecoded || 0 });
    }
  });
  return out;
}
async function phase(base, minFrames, until) {
  const prog = {};
  while (Date.now() < until) {
    await sleep(500);
    if (S.closed) {
      // ノードが落ちた・案内された: 別のノードへ入り直す（SDK と同じ）
      try { S.pc.close(); } catch (e) {}
      const to = S.redirect || q.get('ws2');
      R.rejoins++; log('rejoin via', to);
      S = session(to); base = {};
      await sleep(1000);
      continue;
    }
    if (S.chat.readyState === 'open') S.chat.send(`ping:${name}:${Date.now()}`);
    const f = await frames();
    for (const k of Object.keys(prog)) delete prog[k];
    for (const x of f) if (x.client) prog[x.client] = (prog[x.client] || 0) + Math.max(0, x.decoded - (base[x.mid] || 0));
    const ok = others.every((o) => (prog[o] || 0) >= minFrames && S.pongs.has(o));
    if (ok) return { ok: true, progress: { ...prog }, pongs: [...S.pongs], url: S.url };
  }
  return { ok: false, progress: { ...prog }, pongs: [...S.pongs], url: S.url, slots: S.slots };
}
async function main() {
  stream = await navigator.mediaDevices.getUserMedia({ video: { width: 640, height: 360, frameRate: 30 }, audio: true });
  S = session(q.get('ws'));
  R.p1 = await phase({}, 1, Date.now() + Number(q.get('p1secs') || 60) * 1000);
  window.__p1 = R.p1;
  while (!window.__go) await sleep(200);
  const base = {};
  for (const x of await frames()) base[x.mid] = x.decoded;
  S.pongs = new Set();
  R.p2 = await phase(base, 30, Date.now() + Number(q.get('p2secs') || 90) * 1000);
  window.__p2 = R;
}
main().catch((e) => { R.error = String(e); window.__p1 = window.__p1 || { ok: false, error: String(e) }; window.__p2 = R; });
</script></body></html>
""")
PY
cat > "$W/drive.cjs" <<'EOF'
// node drive.cjs <base> <dir> <query>...: 全ページの __p1 を待って p1.json、<dir>/go ができたら __go、__p2 を待って result.json
const fs = require('fs');
const path = require('path');
const { execSync } = require('child_process');
const [base, dir, ...queries] = process.argv.slice(2);
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
    await page.goto(base + '/?' + q);
    pages.push([name, page]);
    await sleep(1000);
  }
  const collect = async (key, timeout) => {
    const out = {};
    await Promise.all(pages.map(async ([name, page]) => {
      try {
        await page.waitForFunction((k) => window[k] !== undefined, key, { timeout });
        out[name] = await page.evaluate((k) => window[k], key);
      } catch (e) { out[name] = { ok: false, error: 'driver: ' + e.message }; }
      out[name].log = logs[name].slice(-25);
    }));
    return out;
  };
  const p1 = await collect('__p1', 100000);
  fs.writeFileSync(path.join(dir, 'p1.json'), JSON.stringify({ version: browser.version(), results: p1 }, null, 1));
  for (let i = 0; i < 600 && !fs.existsSync(path.join(dir, 'go')); i++) await sleep(200);
  for (const [, page] of pages) await page.evaluate(() => { window.__go = true; });
  const p2 = await collect('__p2', 130000);
  fs.writeFileSync(path.join(dir, 'result.json'), JSON.stringify({ results: p2 }, null, 1));
  await browser.close();
})();
EOF
(cd "$W/www" && exec python3 -m http.server $PP --bind 127.0.0.1 >/dev/null 2>&1) & EXTRA+=("$!")
enc() { python3 -c "import urllib.parse,sys; print(urllib.parse.quote(sys.argv[1], safe=''))" "$1"; }
NAMES=(x alice bob carol)
others() { local n=$1 o=(); for m in alice bob carol; do [ "$m" != "$n" ] && o+=("$m"); done; (IFS=,; echo "${o[*]}"); }
QS=()
for i in 1 2 3; do
  n=${NAMES[$i]}; j=$(( i % 3 + 1 )); [ "$j" = "$H" ] && j=$(( j % 3 + 1 ))
  # 落ちるのは home。入り直す先は home 以外のノード
  [ "$i" = "$H" ] || j=$i
  t=$(tok conf sendrecv "$n")
  QS+=("room=conf&name=$n&others=$(others $n)&p1secs=70&p2secs=100&token=$(enc "$t")&ws=$(enc "ws://127.0.0.1:${HTTP[$i]}/signaling")&ws2=$(enc "ws://127.0.0.1:${HTTP[$j]}/signaling")")
done
echo "alice→n1 bob→n2 carol→n3, home $HOME_N"
timeout 300 docker run --rm --name "$C" --network host -v "$W:$W" "$IMG" bash -c \
  "node '$W/drive.cjs' 'http://127.0.0.1:$PP' '$W' '${QS[0]}' '${QS[1]}' '${QS[2]}'; chown $(id -u):$(id -g) '$W'/*.json 2>/dev/null" >"$W/browser.log" 2>&1 &
DOCKER=$!
for _ in $(seq 600); do [ -s "$W/p1.json" ] && break; kill -0 $DOCKER 2>/dev/null || break; sleep 0.2; done
jq_() { python3 -I -c "import json,sys;d=json.load(open(sys.argv[1]));print(eval(sys.argv[2]))" "$1" "$2"; }
if [ ! -s "$W/p1.json" ]; then fail "no browser result ($(tail -5 "$W/browser.log"))"; summary; exit 1; fi
echo "Chrome $(jq_ "$W/p1.json" 'd["version"]')"
for i in 1 2 3; do
  n=${NAMES[$i]}
  ok=$(jq_ "$W/p1.json" "d['results']['$n'].get('ok')")
  [ "$ok" = True ] && pass "$n on n$i: decoded both others across nodes, DataChannel round trip with $(jq_ "$W/p1.json" "sorted(d['results']['$n'].get('pongs', []))")" \
    || fail "$n on n$i: $(jq_ "$W/p1.json" "{k: v for k, v in d['results']['$n'].items() if k != 'log'}") log=$(jq_ "$W/p1.json" "d['results']['$n'].get('log')")"
done
# REST: どのノードに聞いても home の部屋の核へ中継され、3 人（うち 2 人は別のノード）が見える
ps=$(api $E GET /rooms/conf/participants)
n=$(echo "$ps" | json "['items'].__len__()" 2>/dev/null || echo 0)
nr=$(echo "$ps" | python3 -c "import json,sys; print(len([p for p in json.load(sys.stdin)['items'] if p.get('signaling')=='remote']))" 2>/dev/null || echo 0)
[ "$n" -ge 3 ] && [ "$nr" -ge 2 ] && pass "REST: home lists $n participants ($nr on other nodes)" || fail "REST participants n=$n remote=$nr $(echo "$ps" | head -c 300)"
st=$(api $H GET /nodes | python3 -c "import json,sys; print([x['rooms'] for x in json.load(sys.stdin)['items'] if x.get('self')][0])")
echo "$st" | grep -q "'role': 'home'" && pass "GET /nodes shows the cascade state on the home node" || fail "node status $st"
lc=$(mj "len([x for x in d.get('lifecycle', []) if x[0]=='room.created' and x[1]=='conf'])")
[ "$lc" = 1 ] && pass "edges do not emit room.created again (once for conf)" || fail "room.created for conf: $lc $(mj "d.get('lifecycle')")"
# メディアパイプライン: edge の参加者も書き出す（参加者のいるノードから中継で受ける）
api $E POST /media-pipelines -d '{"name":"conf-export","room":"conf"}' > "$W/mpc.json"; MPC=$(json "['id']" < "$W/mpc.json")
partstreams() { api $H GET "/media-pipelines/$MPC" | python3 -c "import json,sys; print(' '.join(p['participant']+'='+p['stream'] for p in json.load(sys.stdin)['artifacts']['participant_streams']))"; }
nlive=0
for _ in $(seq 60); do
  nlive=0
  for ps in $(partstreams); do [ "$(api $H GET "/streams/${ps#*=}" | json "['observed_state']" 2>/dev/null)" = live ] && nlive=$((nlive+1)); done
  [ "$nlive" -ge 3 ] && break; sleep 0.5
done
[ "$nlive" -ge 3 ] && pass "media pipeline exports all 3 participants, 2 of them on edge nodes ($(partstreams | wc -w) streams live)" || fail "exported live $nlive: $(partstreams)"
EN=${NAMES[$E]}; ESID=$(for ps in $(partstreams); do [ "${ps%%=*}" = "$EN" ] && echo "${ps#*=}"; done; true)
U=$(api $H POST "/streams/$ESID/playback-url" | json "['playback']['hls']" 2>/dev/null || true)
# 最初のセグメントができるまで待つ（書き出しを始めたばかり）
for _ in $(seq 40); do curl -sf "$U" 2>/dev/null | grep -q '#EXTINF' && break; sleep 0.5; done
nf=$({ "$FFMPEG" -v error -t 4 -i "$U" -map 0:v -f framemd5 - 2>/dev/null || true; } | grep -vc "^#" || true)
[ "${nf:-0}" -ge 30 ] && pass "the exported stream of $EN (edge n$E) decodes over HLS ($nf frames)" || fail "edge export frames ${nf:-0} ($U) $(api $H GET "/streams/$ESID" | head -c 800) $(curl -s "$U" | head -c 400)"
# 書き出した stream の配信は home が受けている: 別のノードで録画を頼むと 409 OWNERSHIP_LOST（持ち主の案内つき）
c=$(api $E POST "/streams/$ESID/recordings" -o "$W/own.json" -w '%{http_code}')
[ "$c" = 409 ] && [ "$(json "['code']" < "$W/own.json")" = OWNERSHIP_LOST ] && [ -n "$(json "['details']['owner_api']" < "$W/own.json")" ] \
  && pass "recording on a non-owner node is 409 OWNERSHIP_LOST with the owner's API" || fail "ownership lost $c $(cat "$W/own.json")"
code $E DELETE "/media-pipelines/$MPC" >/dev/null
# home を落とす
kill -9 "${PIDS[$H]}"; wait "${PIDS[$H]}" 2>/dev/null || true; PIDS[$H]=""; QN=$E
touch "$W/go"
wait $DOCKER || true
R="$W/result.json"
if [ ! -s "$R" ]; then fail "no phase-2 result ($(tail -5 "$W/browser.log"))"; summary; exit 1; fi
for i in 1 2 3; do
  n=${NAMES[$i]}
  ok=$(jq_ "$R" "d['results']['$n'].get('p2', {}).get('ok')")
  rj=$(jq_ "$R" "d['results']['$n'].get('rejoins', 0)")
  [ "$ok" = True ] && pass "$n: after the home node died, decodes both others again and DataChannel works (rejoins $rj)" \
    || fail "$n after kill: $(jq_ "$R" "{k: v for k, v in d['results']['$n'].items() if k != 'log'}") log=$(jq_ "$R" "d['results']['$n'].get('log')")"
  if [ "$i" = "$H" ]; then [ "$rj" -ge 1 ] && pass "$n (on the dead node) rejoined via another node" || fail "$n did not rejoin"; fi
done
NEW=$(placed conf)
case "$NEW" in n1|n2|n3) [ "$NEW" != "$HOME_N" ] && pass "conf moved to $NEW";; *) fail "conf placement after kill $NEW";; esac
grep -q "rooms: room conf hosted here (was an edge)" "$W/${NEW}.log" && pass "the new home kept its participants (edge promoted)" || fail "no promotion on $NEW"
# イベント Webhook: 署名・500 での送り直し・全部届く・edge のノードからも届く
ok=0; for _ in $(seq 40); do [ "$(mj "len([k for k,v in d['events'].items() if v is None])")" = 0 ] && { ok=1; break; }; sleep 0.5; done
[ $ok = 1 ] && pass "event webhook: every event delivered after a 500 ($(mj "d['delivered']") events, max attempt $(mj "max(d['event_attempts'].values())"))" || fail "undelivered $(mj "[k for k,v in d['events'].items() if v is None]")"
for t in room.created connection.created track.published connection.destroyed; do
  [ "$(mj "d['event_types'].get('$t', 0)")" -ge 1 ] && pass "event webhook: $t" || fail "event webhook: no $t ($(mj "d['event_types']"))"
done
[ "$(mj "len(d['event_nodes'])")" -ge 3 ] && pass "event webhook: events from all 3 nodes (home and edges)" || fail "event nodes $(mj "d['event_nodes']")"
[ "$(mj "d['bad_signature']")" = 0 ] && pass "event webhook: all deliveries carry a valid signature" || fail "bad signatures $(mj "d['bad_signature']")"
summary
