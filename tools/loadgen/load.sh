#!/usr/bin/env bash
# 負荷試験（churn なし）: 1 つのノードに送り手 PUBLISHERS 本・HLS の視聴者 VIEWERS 人を DURATION 秒。
#
#   PUBLISHERS=4 VIEWERS=200 tools/loadgen/load.sh 60
#
# 中身は tools/soak/soak.sh と同じ（ノードの起動・標本・判定）で、接続の churn を止め、標本の間隔を 5 秒にしたもの。
# 結果: out/load/<時刻>/{load.json,load.txt,samples.csv,analysis.json,run.txt}
set -euo pipefail
ROOT=$(cd "$(dirname "$0")/../.." && pwd)
DURATION=${1:?usage: load.sh DURATION_SECONDS}
export CHURN=0 PUB_CHURN=0
export INTERVAL=${INTERVAL:-5}
export PUBLISHERS=${PUBLISHERS:-4} VIEWERS=${VIEWERS:-200}
export SIZE=${SIZE:-1920x1080} BITRATE=${BITRATE:-4000}
export WARMUP=${WARMUP:-$(( DURATION / 4 ))}
# 負荷試験では RSS の増分を判定しない（ライブの窓が埋まるまで RSS は増える。判定は soak.sh の長時間で行う）
export MAX_GROWTH=${MAX_GROWTH:-100000}
export OUT=${OUT:-$ROOT/out/load/$(date -u +%Y%m%dT%H%M%SZ)}
exec "$ROOT/tools/soak/soak.sh" "$DURATION"
