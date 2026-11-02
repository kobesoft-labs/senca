#!/usr/bin/env bash
# M8: ノードの WHIP（Chrome から送る）と WHEP（Chrome で見る）。Chrome は Docker（ov-chrome:latest、playwright-core）。
source "$(dirname "$0")/lib.sh"
IMG=${CHROME_IMAGE:-ov-chrome:latest}
docker image inspect "$IMG" >/dev/null 2>&1 || { echo "SKIP (no $IMG)"; exit 0; }
[ -n "${NODE_BIN_PREBUILT:-}" ] || (cd "$ROOT" && cargo build -q -p senca-node)
W=$(mktemp -d -p "${HOME}/.cache"); NODE_PID=; WEB_PID=
cleanup() { [ -n "$NODE_PID" ] && kill "$NODE_PID" 2>/dev/null; [ -n "$WEB_PID" ] && kill "$WEB_PID" 2>/dev/null; docker rm -f senca-nwrtc >/dev/null 2>&1; wait 2>/dev/null; [ -n "${KEEP:-}" ] && echo "kept $W" || rm -rf "$W"; }
trap cleanup EXIT
HTTP=$(port); RTMP=$(port); UDP=$(port); WEB=$(port)
"$NODE_BIN" --data-dir "$W/data" --http 127.0.0.1:$HTTP --rtmp 127.0.0.1:$RTMP --webrtc-udp 127.0.0.1:$UDP \
  --public-http-url http://127.0.0.1:$HTTP --reconnect-grace 3000 >"$W/node.log" 2>&1 &
NODE_PID=$!
for _ in $(seq 50); do curl -sf http://127.0.0.1:$HTTP/readyz >/dev/null && break; sleep 0.1; done
TOKEN=$(cat "$W/data/control/bootstrap-admin-token"); API=http://127.0.0.1:$HTTP/api/v1
api() { local m=$1 p=$2; shift 2; curl -s -X "$m" -H "Authorization: Bearer $TOKEN" -H 'Content-Type: application/json' "$API$p" "$@"; }

sed -n '/^cat > "\$W\/drive.cjs"/,/^EOF$/p' "$ROOT/tools/interop/webrtc.sh" | sed '1d;$d' > "$W/drive.cjs"
mkdir -p "$W/web"
cat > "$W/web/whip.html" <<'H'
<!doctype html><script type="module">
const q = new URLSearchParams(location.search);
(async () => {
  try {
    const s = await navigator.mediaDevices.getUserMedia({ video: { width: 640, height: 360 }, audio: true });
    const pc = new RTCPeerConnection();
    for (const t of s.getTracks()) {
      const tr = pc.addTransceiver(t, { direction: 'sendonly' });
      if (t.kind === 'video') {
        const c = RTCRtpSender.getCapabilities('video').codecs.filter(c => c.mimeType === 'video/H264' && /packetization-mode=1/.test(c.sdpFmtpLine || ''));
        if (c.length) tr.setCodecPreferences(c);
      }
    }
    await pc.setLocalDescription(await pc.createOffer());
    const r = await fetch(q.get('whip'), { method: 'POST', headers: { 'Content-Type': 'application/sdp', Authorization: 'Bearer ' + q.get('key') }, body: pc.localDescription.sdp });
    if (r.status !== 201) throw new Error('whip ' + r.status + ' ' + await r.text());
    const loc = r.headers.get('Location');
    await pc.setRemoteDescription({ type: 'answer', sdp: await r.text() });
    await new Promise(res => setTimeout(res, 10000));
    let frames = 0;
    (await pc.getStats()).forEach(x => { if (x.type === 'outbound-rtp' && x.kind === 'video') frames += x.framesEncoded || 0; });
    window.__result = { ok: true, frames, state: pc.connectionState, location: loc };
  } catch (e) { window.__result = { ok: false, error: String(e) }; }
})();
</script>
H
cat > "$W/web/whep.html" <<'H'
<!doctype html><video autoplay muted playsinline></video><script type="module">
const q = new URLSearchParams(location.search);
(async () => {
  try {
    const pc = new RTCPeerConnection();
    pc.addTransceiver('video', { direction: 'recvonly' });
    pc.addTransceiver('audio', { direction: 'recvonly' });
    pc.ontrack = (e) => { document.querySelector('video').srcObject = e.streams[0] || new MediaStream([e.track]); };
    await pc.setLocalDescription(await pc.createOffer());
    const r = await fetch(q.get('whep'), { method: 'POST', headers: { 'Content-Type': 'application/sdp' }, body: pc.localDescription.sdp });
    if (r.status !== 201) throw new Error('whep ' + r.status + ' ' + await r.text());
    await pc.setRemoteDescription({ type: 'answer', sdp: await r.text() });
    await new Promise(res => setTimeout(res, 8000));
    let decoded = 0, dropped = 0, w = 0;
    (await pc.getStats()).forEach(x => { if (x.type === 'inbound-rtp' && x.kind === 'video') { decoded = x.framesDecoded || 0; dropped = x.framesDropped || 0; w = x.frameWidth || 0; } });
    window.__result = { ok: true, decoded, dropped, width: w, state: pc.connectionState };
  } catch (e) { window.__result = { ok: false, error: String(e) }; }
})();
</script>
H
(cd "$W/web" && exec python3 -m http.server $WEB --bind 127.0.0.1 >/dev/null 2>&1) & WEB_PID=$!
sleep 0.5
browser() { docker run --rm --name senca-nwrtc --network host -v "$W:$W" "$IMG" bash -c "node '$W/drive.cjs' '$1' '$W/$2.json' 30000; chown $(id -u):$(id -g) '$W/$2.json'" >"$W/$2.log" 2>&1 || true; }
jq_() { python3 -c "import json,sys;d=json.load(open(sys.argv[1]));print(eval(sys.argv[2]))" "$1" "$2"; }

# 1. WHIP: Chrome（偽のカメラ、H.264＋Opus）→ ノード → ライブ HLS
S=$(api POST /streams -d '{"name":"whip"}'); SID=$(echo "$S" | json "['id']"); KEY=$(echo "$S" | json "['ingest']['stream_key']")
WHIP=$(python3 -c "import urllib.parse,sys;print(urllib.parse.quote(sys.argv[1],safe=''))" "http://127.0.0.1:$HTTP/whip/$SID")
browser "http://127.0.0.1:$WEB/whip.html?whip=$WHIP&key=$KEY" whip
[ "$(jq_ "$W/whip.json" "d['ok']")" = True ] && pass "WHIP: Chrome published ($(jq_ "$W/whip.json" "d['frames']") frames encoded)" || fail "WHIP: $(cat "$W/whip.json")"
sleep 4
[ -n "${DEBUG:-}" ] && api GET /streams/$SID > "$W/whip-stream.json"
URL=$(api POST /streams/$SID/playback-url | json "['playback']['hls']")
[ -n "${DEBUG:-}" ] && curl -s "$URL" > "$W/whip.m3u8"
n=$(frames "$URL" v | wc -l)
[ "$n" -ge 60 ] && pass "WHIP → live HLS decodes ($n frames)" || fail "WHIP → HLS frames $n"
api GET /streams/$SID | grep -q '"transport":"webrtc"\|observed_state' && pass "WHIP stream registered" || fail "WHIP stream state"
# 鍵が違えば 401
c=$(curl -s -o /dev/null -w '%{http_code}' -X POST -H 'Content-Type: application/sdp' -H 'Authorization: Bearer sk_bogus' --data-binary 'v=0' "http://127.0.0.1:$HTTP/whip/$SID")
[ "$c" = 401 ] && pass "WHIP bad key 401" || fail "WHIP bad key $c"

# 2. WHEP: ffmpeg（RTMP、H.264）→ ノード → Chrome
S=$(api POST /streams -d '{"name":"whep"}'); SID2=$(echo "$S" | json "['id']"); KEY2=$(echo "$S" | json "['ingest']['stream_key']")
"$FFMPEG" -v error -re -f lavfi -i testsrc2=size=640x360:rate=30 -t 25 -c:v libx264 -profile:v baseline -g 30 -bf 0 -f flv rtmp://127.0.0.1:$RTMP/live/$KEY2 & PUB=$!
sleep 3
WHEPU=$(api POST /streams/$SID2/playback-url | json "['playback']['whep']")
browser "http://127.0.0.1:$WEB/whep.html?whep=$(python3 -c "import urllib.parse,sys;print(urllib.parse.quote(sys.argv[1],safe=''))" "$WHEPU")" whep
d=$(jq_ "$W/whep.json" "d.get('decoded',0)")
[ "$(jq_ "$W/whep.json" "d['ok']")" = True ] && [ "$d" -ge 100 ] && pass "WHEP: Chrome decoded $d frames ($(jq_ "$W/whep.json" "d.get('width')") wide, dropped $(jq_ "$W/whep.json" "d.get('dropped')"))" || fail "WHEP: $(cat "$W/whep.json")"
kill $PUB 2>/dev/null; wait $PUB 2>/dev/null || true
c=$(curl -s -o /dev/null -w '%{http_code}' -X POST -H 'Content-Type: application/sdp' --data-binary 'v=0' "http://127.0.0.1:$HTTP/whep/$SID2?exp=1&sig=AAAA")
[ "$c" = 403 ] && pass "WHEP bad signature 403" || fail "WHEP bad signature $c"
summary
