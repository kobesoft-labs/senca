#!/usr/bin/env bash
# WebRTC（senca-webrtc・senca-sctp）とブラウザー（Docker の Google Chrome ＋ playwright-core）の相互接続試験。
#
#   A〜C WHIP: Chrome の getUserMedia（偽のカメラ・マイク）→ sendonly → POST offer → senca の answer →
#        受けた AU をファイル（VP8 は IVF、H.264 は Annex B、Opus は Ogg）に書き、FFmpeg で誤りなく復号できること。
#        DataChannel の echo（文字列・空文字列・バイナリ）、trickle の PATCH、simulcast（3 つの rid）。
#   D〜G WHEP: FFmpeg で作った VP8・H.264・AV1 を senca から Chrome（recvonly）へ。getStats の framesDecoded・
#        framesDropped・破損フレーム。client offer と server offer。
#   H    損失のある UDP 中継（server → Chrome の media を 5 % 落とす）を挟み、Chrome の NACK に RTX で応えること。
#
# 使い方: tools/interop/webrtc.sh（KEEP=1 で作業場所を残す、BROWSER_IMAGE=... で Chrome のイメージを選ぶ）
# ポートは 50000〜50999 を使う。Chrome は --network host で動かす。
set -euo pipefail
source "$(dirname "$0")/lib.sh"
CARGO=${CARGO:-$HOME/.cargo/bin/cargo}
export CARGO_TARGET_DIR=${CARGO_TARGET_DIR:-$HOME/.cache/senca-target-webrtc}
mkdir -p "$HOME/.cache"
W=$(mktemp -d -p "$HOME/.cache" senca-webrtc-interop.XXXXXX)
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

[ -n "${SKIP_BUILD:-}" ] || (cd "$ROOT" && "$CARGO" build -q -p senca-webrtc --examples)
EX=$CARGO_TARGET_DIR/debug/examples

IMG=${BROWSER_IMAGE:-ov-chrome:latest}
if ! docker image inspect "$IMG" >/dev/null 2>&1; then
  echo "SKIP: Docker image $IMG がない"; exit 0
fi
# Chrome から見えるこの機械のアドレス（UDP の候補）。ページは安全な文脈（getUserMedia）のため 127.0.0.1 で開く。
IP=${HOST_IP:-$(hostname -I | awk '{print $1}')}
BASE=$((50000 + (RANDOM % 90) * 10))
port() { BASE=$((BASE + 1)); P=$BASE; }

# ------------------------------------------------------------------ 道具
cat > "$W/drive.cjs" <<'EOF'
// node drive.cjs <url> <out.json> <timeout_ms>
const fs = require('fs');
const path = require('path');
const { execSync } = require('child_process');
const [url, out, timeout] = process.argv.slice(2);
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
  const page = await ctx.newPage();
  const logs = [];
  page.on('console', (m) => logs.push(m.type() + ': ' + m.text()));
  page.on('pageerror', (e) => logs.push('pageerror: ' + e.message));
  let result;
  try {
    await page.goto(url);
    await page.waitForFunction(() => window.__result !== undefined, null, { timeout: Number(timeout) });
    result = await page.evaluate(() => window.__result);
  } catch (e) {
    result = { ok: false, error: 'driver: ' + e.message };
  }
  result.version = browser.version();
  // favicon の 404 は除く
  result.consoleErrors = logs.filter((l) => (l.startsWith('error') || l.startsWith('pageerror')) && !/status of 404/.test(l));
  fs.writeFileSync(out, JSON.stringify(result, null, 1));
  await browser.close();
})();
EOF

# 損失のある UDP 中継: <listen ip:port> <server ip:port> <loss>
# Chrome → server はそのまま、server → Chrome の SRTP（RTCP を除く 200 byte 超）を loss の割合で落とす。
cat > "$W/relay.py" <<'EOF'
import random, select, socket, sys, json
lip, lport = sys.argv[1].rsplit(':', 1)
sip, sport = sys.argv[2].rsplit(':', 1)
loss = float(sys.argv[3]); stats_path = sys.argv[4]
a = socket.socket(socket.AF_INET, socket.SOCK_DGRAM); a.bind((lip, int(lport)))
b = socket.socket(socket.AF_INET, socket.SOCK_DGRAM); b.bind((lip, 0))
server = (sip, int(sport)); chrome = None
rnd = random.Random(7); st = {'to_server': 0, 'to_chrome': 0, 'dropped': 0}
while True:
    r, _, _ = select.select([a, b], [], [], 1.0)
    for s in r:
        d, src = s.recvfrom(65536)
        if s is a:
            chrome = src; b.sendto(d, server); st['to_server'] += 1
        elif chrome:
            media = 128 <= d[0] <= 191 and not (192 <= d[1] <= 223) and len(d) > 200
            if media and rnd.random() < loss:
                st['dropped'] += 1
            else:
                a.sendto(d, chrome); st['to_chrome'] += 1
    with open(stats_path, 'w') as f: json.dump(st, f)
EOF

browser() { # name url timeout_ms → $W/<name>.json
  local name=$1 url=$2 t=$3 c="senca-webrtc-$$-$1"
  CONTAINERS+=("$c")
  timeout $((t / 1000 + 60)) docker run --rm --name "$c" --network host -v "$W:$W" "$IMG" bash -c \
    "node '$W/drive.cjs' '$url' '$W/$name.json' $t; chown $(id -u):$(id -g) '$W/$name.json'" >"$W/$name.browser.log" 2>&1 || true
  [ -s "$W/$name.json" ] || echo '{"ok":false,"error":"no result"}' > "$W/$name.json"
}

jq_() { python3 -I -c "import json,sys;d=json.load(open(sys.argv[1]));print(eval(sys.argv[2]))" "$1" "$2"; }
wait_http() { local i; for i in $(seq 100); do curl -sf "$1" >/dev/null && return 0; sleep 0.1; done; return 1; }

# FFmpeg で誤りなく復号でき、フレーム（または音声の packet）が $2 以上
decodes() { # file min_frames label
  local f=$1 min=$2 label=$3 err n
  # 出力の時間軸は入力のまま（null muxer の既定 1/fps では可変フレームレートの時刻が重なることがある）
  err=$("$FFMPEG" -v error -i "$f" -enc_time_base demux -f null - 2>&1 | head -3 || true)
  n=$("$FFPROBE" -v error -count_packets -select_streams 0 -show_entries stream=nb_read_packets -of csv=p=0 "$f" 2>/dev/null | head -1)
  n=${n:-0}
  if [ -z "$err" ] && [ "$n" -ge "$min" ]; then pass "$label: $(basename "$f") decodes cleanly ($n frames/packets)"
  else fail "$label: $(basename "$f") n=$n err=${err:0:200}"; fi
}
max_width() { "$FFPROBE" -v error -show_entries frame=width -of csv=p=0 "$1" 2>/dev/null | sort -n | tail -1; }

echo "browser image: $IMG, host address: $IP, ports from $BASE"

# ------------------------------------------------------------------ A〜C WHIP
whip_case() { # label query min_video
  local label=$1 query=$2 min=$3 out="$W/whip-$1" hp up r
  mkdir -p "$out"
  port; hp=$P; port; up=$P
  "$EX/whip_server" --http "127.0.0.1:$hp" --udp "$IP:$up" --out "$out" >"$W/whip-$label.log" 2>&1 &
  local pid=$!; PIDS+=("$pid")
  wait_http "http://127.0.0.1:$hp/status" || { fail "$label: server did not start"; return; }
  browser "whip-$label" "http://127.0.0.1:$hp/?$query" 60000
  curl -s "http://127.0.0.1:$hp/status" > "$W/whip-$label.status.json" || true
  kill "$pid" 2>/dev/null || true; wait "$pid" 2>/dev/null || true
  r="$W/whip-$label.json"
  if [ "$(jq_ "$r" 'd["ok"]')" = True ]; then
    pass "$label: Chrome $(jq_ "$r" 'd["version"]') connected, WHIP POST/PATCH(trickle $(jq_ "$r" 'd["patchStatus"]'))/DELETE($(jq_ "$r" 'd["deleteStatus"]'))"
  else
    fail "$label: browser $(jq_ "$r" 'd.get("error")')"; return
  fi
  [ "$(jq_ "$r" 'len(d["consoleErrors"])')" = 0 ] && pass "$label: no console errors" || fail "$label: console errors $(jq_ "$r" 'd["consoleErrors"]')"
  echo "  outbound: $(jq_ "$r" '[(o.get("rid"), o.get("frameWidth"), o.get("framesEncoded"), o.get("encoderImplementation")) for o in d["outbound"] if o.get("kind")=="video"]') availableOutgoingBitrate=$(jq_ "$r" 'd["availableOutgoingBitrate"]')"
  WHIP_OUT=$out; WHIP_JSON=$r
}

check_dc() { # label
  local r=$WHIP_JSON label=$1
  if [ "$(jq_ "$r" 'd["dcOpen"]')" = True ] && [ "$(jq_ "$r" 'd["echoes"]')" = "['echo:hello', 'echo:こんにちは', 'echo:', 'binary:70000']" ]; then
    pass "$label: DataChannel echo (text, UTF-8, empty string, 70000-byte binary)"
  else
    fail "$label: DataChannel dcOpen=$(jq_ "$r" 'd["dcOpen"]') echoes=$(jq_ "$r" 'd["echoes"]')"
  fi
}

whip_case A "codec=vp8&secs=8" 150
if [ -n "${WHIP_OUT:-}" ]; then
  decodes "$WHIP_OUT/1_0_main.ivf" 150 "A VP8"
  decodes "$WHIP_OUT/1_1_main.ogg" 300 "A Opus"
  check_dc A
fi
WHIP_OUT=
whip_case B "codec=h264&secs=8" 150
if [ -n "${WHIP_OUT:-}" ]; then
  decodes "$WHIP_OUT/1_0_main.h264" 150 "B H.264"
  echo "  codec: $(jq_ "$WHIP_JSON" '[o for o in d["outbound"] if "codec" in o]')"
fi
WHIP_OUT=
whip_case C "codec=vp8&simulcast=1&secs=12" 100
if [ -n "${WHIP_OUT:-}" ]; then
  ws=()
  for rid in q h f; do
    f="$WHIP_OUT/1_0_$rid.ivf"
    if [ -s "$f" ]; then decodes "$f" 100 "C simulcast rid=$rid"; ws+=("$(max_width "$f")"); else fail "C simulcast: no file for rid=$rid"; ws+=(0); fi
  done
  if [ "${ws[0]}" -lt "${ws[1]}" ] && [ "${ws[1]}" -lt "${ws[2]}" ]; then pass "C simulcast layers have increasing widths ${ws[*]}"
  else fail "C simulcast widths ${ws[*]}"; fi
fi

# ------------------------------------------------------------------ D〜H WHEP
"$FFMPEG" -v error -y -f lavfi -i testsrc2=size=640x360:rate=30 -t 6 -c:v libvpx -b:v 800k -g 60 \
  -deadline realtime -auto-alt-ref 0 -lag-in-frames 0 "$W/in.ivf"
"$FFMPEG" -v error -y -f lavfi -i testsrc2=size=640x360:rate=30 -t 6 -c:v libx264 -profile:v baseline -pix_fmt yuv420p \
  -tune zerolatency -bf 0 -g 60 -x264-params aud=1 -f h264 "$W/in.h264"
"$FFMPEG" -v error -y -f lavfi -i testsrc2=size=640x360:rate=30 -t 6 -c:v libaom-av1 -usage realtime -cpu-used 8 \
  -g 60 -b:v 600k "$W/in-av1.ivf"

whep_case() { # label codec input query [loss]
  local label=$1 codec=$2 input=$3 query=$4 loss=${5:-} hp up rp adv r
  port; hp=$P; port; up=$P
  adv=$IP:$up
  if [ -n "$loss" ]; then
    port; rp=$P; adv=$IP:$rp
    python3 -I "$W/relay.py" "$adv" "$IP:$up" "$loss" "$W/relay-$label.json" >"$W/relay-$label.log" 2>&1 &
    PIDS+=("$!")
  fi
  local extra=()
  [[ $query == *offer=server* ]] && extra=(--offer server)
  "$EX/whep_server" --http "127.0.0.1:$hp" --udp "$IP:$up" --advertise "$adv" --input "$input" --codec "$codec" \
    "${extra[@]}" >"$W/whep-$label.log" 2>&1 &
  local pid=$!; PIDS+=("$pid")
  wait_http "http://127.0.0.1:$hp/status" || { fail "$label: server did not start ($(tail -2 "$W/whep-$label.log"))"; return; }
  browser "whep-$label" "http://127.0.0.1:$hp/?$query" 60000
  curl -s "http://127.0.0.1:$hp/status" > "$W/whep-$label.status.json" || true
  kill "$pid" 2>/dev/null || true; wait "$pid" 2>/dev/null || true
  r="$W/whep-$label.json"
  if [ "$(jq_ "$r" 'd["ok"]')" != True ]; then fail "$label: browser $(jq_ "$r" 'd.get("error")')"; return; fi
  local dec rcv drop corrupt key lost nack
  dec=$(jq_ "$r" 'd["framesDecoded"]'); rcv=$(jq_ "$r" 'd["framesReceived"]'); drop=$(jq_ "$r" 'd["framesDropped"]')
  corrupt=$(jq_ "$r" 'd["corruptedVideoFrames"]'); key=$(jq_ "$r" 'd["keyFramesDecoded"]')
  lost=$(jq_ "$r" 'd["packetsLost"]'); nack=$(jq_ "$r" 'd["nackCount"]')
  echo "  $label: $(jq_ "$r" 'd["codec"]') $(jq_ "$r" 'd["frameWidth"]')x$(jq_ "$r" 'd["frameHeight"]') decoded=$dec received=$rcv dropped=$drop keyframes=$key lost=$lost nack=$nack pli=$(jq_ "$r" 'd["pliCount"]') rtx=$(jq_ "$r" 'd.get("retransmittedPacketsReceived")') freezes=$(jq_ "$r" 'd["freezeCount"]') decoder=$(jq_ "$r" 'd.get("decoderImplementation")')"
  local want=$(( $(jq_ "$r" 'd.get("secs", 8)' 2>/dev/null || echo 8) ))
  if [ "$dec" -ge 200 ] && [ "$corrupt" = 0 ] && [ "$key" -ge 1 ]; then
    pass "$label: Chrome decoded $dec frames of $(jq_ "$r" 'd["codec"]') (dropped $drop, corrupted 0)"
  else
    fail "$label: decoded=$dec corrupted=$corrupt keyframes=$key"
  fi
  [ "$(jq_ "$r" 'len(d["consoleErrors"])')" = 0 ] && pass "$label: no console errors" || fail "$label: console errors $(jq_ "$r" 'd["consoleErrors"]')"
  WHEP_JSON=$r; WHEP_STATUS="$W/whep-$label.status.json"
  : "$want"
}

whep_case D vp8 "$W/in.ivf" "secs=10"
whep_case E h264 "$W/in.h264" "secs=10"
whep_case F av1 "$W/in-av1.ivf" "secs=10"
whep_case G vp8 "$W/in.ivf" "secs=10&offer=server"
whep_case H vp8 "$W/in.ivf" "secs=12" 0.05
if [ -s "${WHEP_STATUS:-/nonexistent}" ] && [[ $WHEP_STATUS == *whep-H* ]]; then
  nack=$(jq_ "$WHEP_JSON" 'd["nackCount"]'); rtx=$(jq_ "$WHEP_STATUS" 'd[0]["retransmitted"]')
  rrx=$(jq_ "$WHEP_JSON" 'd.get("retransmittedPacketsReceived") or 0'); lost=$(jq_ "$WHEP_JSON" 'd["packetsLost"]')
  dropped=$(jq_ "$W/relay-H.json" 'd["dropped"]')
  echo "  H: relay dropped $dropped media packets; Chrome sent $nack NACKs, received $rrx retransmissions; senca retransmitted $rtx; Chrome packetsLost=$lost"
  if [ "$nack" -gt 0 ] && [ "$rtx" -gt 0 ] && [ "$rrx" -gt 0 ]; then pass "H: Chrome's NACKs answered with RTX ($rtx retransmitted, $rrx received)"
  else fail "H: nack=$nack retransmitted=$rtx rtx_received=$rrx"; fi
  # Chrome の packetsLost は RTX で戻した packet も数える。戻せたかは PLI が要らず全フレームを復号できたかで見る
  pli=$(jq_ "$WHEP_JSON" 'd["pliCount"]'); dec=$(jq_ "$WHEP_JSON" 'd["framesDecoded"]')
  if [ "$pli" = 0 ] && [ "$dec" -ge $((12 * 30 * 95 / 100)) ]; then pass "H: all losses repaired by RTX (no PLI, $dec frames decoded in 12 s)"
  else fail "H: pli=$pli decoded=$dec"; fi
fi
summary
