#!/usr/bin/env bash
# 長時間の安定性試験（AC-21: 72 時間・接続の churn・crash ゼロ・warm-up 後の RSS 増分 5% 以内・資源の回収）。
#
#   tools/soak/soak.sh DURATION_SECONDS          # 例: 180（開発機の短い版）、259200（72 時間、release の gate）
#
# 1 つのノードを起動し、送り手（FFmpeg の RTMP、つなぎ直しあり）と HLS の視聴者（接続・切断を繰り返す）を
# DURATION 秒流す。ノードの RSS・CPU・fd・socket・スレッドを INTERVAL 秒ごとに CSV に書き、最後に判定する:
#   - ノードが落ちていない（プロセスが残っている・ログに panic がない）
#   - warm-up（WARMUP 秒、既定は DURATION の 1/3 と 600 秒の小さい方）の後の RSS 増分が MAX_GROWTH%（既定 5）以内
#   - 負荷を止めた後の fd・socket が負荷の前と同程度（FD_SLACK 個まで）
#
# 環境変数: NODE_BIN CODEC_WORKER_BIN PUBLISHERS(4) VIEWERS(50) CHURN(30: 視聴の平均秒) PUB_CHURN(60: 送り手の
#   つなぎ直しの平均間隔秒) INTERVAL(10) WARMUP MAX_GROWTH(5) FD_SLACK(4) BITRATE(2000) SIZE(1280x720) RECORD(0)
#   PROCS（視聴者のプロセス数） OUT（結果の置き場所、既定 out/soak/<時刻>）
set -euo pipefail
ROOT=$(cd "$(dirname "$0")/../.." && pwd)
CACHE=${XDG_CACHE_HOME:-$HOME/.cache}
DURATION=${1:?usage: soak.sh DURATION_SECONDS}
NODE_BIN=${NODE_BIN:-$CACHE/senca-target-release/release/senca-node}
CODEC_WORKER_BIN=${CODEC_WORKER_BIN:-$(dirname "$NODE_BIN")/senca-codec-worker}
FFMPEG=${FFMPEG:-$HOME/tools/ffmpeg}
PUBLISHERS=${PUBLISHERS:-4}
VIEWERS=${VIEWERS:-50}
CHURN=${CHURN:-30}
PUB_CHURN=${PUB_CHURN:-60}
INTERVAL=${INTERVAL:-10}
WARMUP=${WARMUP:-$(( DURATION / 3 < 600 ? DURATION / 3 : 600 ))}
MAX_GROWTH=${MAX_GROWTH:-5}
FD_SLACK=${FD_SLACK:-4}
BITRATE=${BITRATE:-2000}
SIZE=${SIZE:-1280x720}
STAMP=$(date -u +%Y%m%dT%H%M%SZ)
OUT=${OUT:-$ROOT/out/soak/$STAMP}
LOADGEN="python3 -I $ROOT/tools/loadgen/loadgen.py"
mkdir -p "$OUT"
W=$(mktemp -d "$CACHE/senca-soak.XXXXXX")
NODE_PID= SAMPLER_PID= LOAD_PID=
cleanup() {
  for p in $LOAD_PID $SAMPLER_PID; do kill "$p" 2>/dev/null || true; done
  [[ -n "$NODE_PID" ]] && kill "$NODE_PID" 2>/dev/null || true
  wait 2>/dev/null || true
  [[ -n "${KEEP:-}" ]] && echo "soak: 作業場所 $W" || rm -rf "$W"
}
trap cleanup EXIT INT TERM

[[ -x "$NODE_BIN" ]] || { echo "soak: $NODE_BIN がない（cargo build --release -p senca-node -p senca-codec-worker）" >&2; exit 2; }
port() { python3 -I -c 'import socket;s=socket.socket();s.bind(("127.0.0.1",0));print(s.getsockname()[1])'; }
HTTP=$(port); RTMP=$(port)
ulimit -n 65536 2>/dev/null || ulimit -n "$(ulimit -Hn)"

"$NODE_BIN" --data-dir "$W/data" --http 127.0.0.1:$HTTP --rtmp 127.0.0.1:$RTMP --srt off \
  --public-rtmp-url rtmp://127.0.0.1:$RTMP --public-http-url http://127.0.0.1:$HTTP \
  --codec-worker-bin "$CODEC_WORKER_BIN" --codec-workers 2 --max-playback-ttl 86400 >"$OUT/node.log" 2>&1 &
NODE_PID=$!
for _ in $(seq 100); do curl -sf "http://127.0.0.1:$HTTP/readyz" >/dev/null && break; sleep 0.1; done
curl -sf "http://127.0.0.1:$HTTP/readyz" >/dev/null || { echo "soak: ノードが起動しない"; tail "$OUT/node.log"; exit 1; }

echo idle-before >"$W/phase"
$LOADGEN sample --pid "$NODE_PID" --csv "$OUT/samples.csv" --interval "$INTERVAL" --phase-file "$W/phase" &
SAMPLER_PID=$!
{
  echo "start=$STAMP duration=$DURATION warmup=$WARMUP publishers=$PUBLISHERS viewers=$VIEWERS churn=$CHURN pub_churn=$PUB_CHURN"
  echo "node=$NODE_BIN $(sha256sum "$NODE_BIN" | cut -c1-16) commit=$(git -C "$ROOT" rev-parse --short HEAD)$(git -C "$ROOT" diff --quiet || echo +dirty)"
  echo "host=$(uname -srm) cpu=$(sed -n 's/^model name\s*: //p' /proc/cpuinfo | head -1) cores=$(nproc) mem=$(free -g | awk '/Mem:/{print $2}')GiB"
} | tee "$OUT/run.txt"
sleep $(( INTERVAL * 2 + 1 ))

echo load >"$W/phase"
set +e
$LOADGEN scenario --api "http://127.0.0.1:$HTTP/api/v1" --token-file "$W/data/control/bootstrap-admin-token" \
  --rtmp "rtmp://127.0.0.1:$RTMP" --ffmpeg "$FFMPEG" --publishers "$PUBLISHERS" --viewers "$VIEWERS" \
  --duration "$DURATION" --churn "$CHURN" --pub-churn "$PUB_CHURN" --size "$SIZE" --bitrate "$BITRATE" \
  ${PROCS:+--procs "$PROCS"} ${RECORD:+$( [[ "$RECORD" == 1 ]] && echo --record )} \
  --work "$CACHE/senca-loadgen" --out "$OUT/load.json" >"$OUT/load.txt" 2>&1 &
LOAD_PID=$!
wait "$LOAD_PID"
LOAD_RC=$?
LOAD_PID=
set -e
cat "$OUT/load.txt"

# 負荷を止めた後: 再接続の猶予・keep-alive の期限を待ってから資源を数える
echo idle-after >"$W/phase"
sleep $(( INTERVAL * 3 + 15 ))
kill "$SAMPLER_PID" 2>/dev/null || true
wait "$SAMPLER_PID" 2>/dev/null || true
SAMPLER_PID=

ALIVE=1
kill -0 "$NODE_PID" 2>/dev/null || ALIVE=0
PANICS=$(grep -c -i 'panicked' "$OUT/node.log" || true)
set +e
$LOADGEN analyze --csv "$OUT/samples.csv" --warmup "$WARMUP" --max-growth "$MAX_GROWTH" --fd-slack "$FD_SLACK" \
  --out "$OUT/analysis.json"
AN_RC=$?
set -e

echo "== soak のまとめ（$OUT）"
res=PASS
[[ $ALIVE == 1 ]] && echo "PASS ノードが動き続けた" || { echo "FAIL ノードが落ちた"; res=FAIL; }
[[ $PANICS == 0 ]] && echo "PASS panic なし" || { echo "FAIL panic が $PANICS 件（node.log）"; res=FAIL; }
[[ $AN_RC == 0 ]] && echo "PASS RSS 増分・fd/socket の回収（analysis.json）" || { echo "FAIL RSS・資源（analysis.json）"; res=FAIL; }
[[ $LOAD_RC == 0 ]] && echo "PASS 視聴者の誤りなし" || echo "WARN 視聴者の誤りあり（load.json。churn の切断の瞬間を含む）"
echo "$res"
[[ $res == PASS ]]
