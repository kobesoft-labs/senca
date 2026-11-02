#!/usr/bin/env bash
# 全 fuzz target を SECONDS 秒ずつ回し、まとめを出す。
#
#   tools/fuzz/all.sh SECONDS            # CI の smoke は 60。release 候補の gate は 86400（24 時間、仕様 15.3）
#   PARALLEL=4 tools/fuzz/all.sh 60      # 同時に回す数（既定 2。RSS 上限 × 並列数のメモリーが要る）
#   TARGETS="rtmp_server srt_listener" tools/fuzz/all.sh 600
#
# まとめは fuzz/artifacts/summary.md（と results.tsv）。どれかが FAIL なら終了コード 1。
set -euo pipefail
source "$(dirname "$0")/common.sh"

SECS=${1:?usage: all.sh SECONDS}
PARALLEL=${PARALLEL:-2}
TARGETS=${TARGETS:-$(fuzz_targets | tr '\n' ' ')}

mkdir -p "$FUZZ_OUT"
STAMP=$(date -u +%Y%m%dT%H%M%SZ)
: >"$FUZZ_OUT/results.tsv"
pids=()
cleanup() {
  for p in "${pids[@]:-}"; do [[ -n "$p" ]] && kill "$p" 2>/dev/null || true; done
}
trap cleanup EXIT INT TERM

echo "== build（sanitizer: $FUZZ_SANITIZER）"
fuzz_build

if [[ ! -d "$CORPUS" ]] || [[ -z "$(ls -A "$CORPUS" 2>/dev/null)" ]]; then
  echo "== 種がないので作る"
  "$ROOT/tools/fuzz/seed.sh"
fi

echo "== 実行: $(wc -w <<<"$TARGETS") target × ${SECS}s（同時 $PARALLEL）"
for t in $TARGETS; do
  while (( $(jobs -rp | wc -l) >= PARALLEL )); do wait -n || true; done
  SKIP_BUILD=1 "$ROOT/tools/fuzz/run.sh" "$t" "$SECS" >/dev/null 2>&1 &
  pids+=($!)
done
wait || true
pids=()

SUM=$FUZZ_OUT/summary.md
{
  echo "# fuzz のまとめ（$STAMP、${SECS} 秒 / target、sanitizer: $FUZZ_SANITIZER）"
  echo
  echo "- commit: $(git -C "$ROOT" rev-parse --short HEAD 2>/dev/null || echo '?')$(git -C "$ROOT" diff --quiet 2>/dev/null || echo ' (作業中の変更あり)')"
  echo "- rustc: $(PATH="$HOME/.cargo/bin:$PATH" rustc +nightly --version 2>/dev/null || echo '?')"
  echo
  echo "| target | 結果 | 実行数 | exec/s | cov（edge） | ft | corpus | peak RSS MB | 再現入力 |"
  echo "|---|---|---:|---:|---:|---:|---:|---:|---|"
  sort "$FUZZ_OUT/results.tsv" | while IFS=$'\t' read -r t res s ex eps cov ft corp rss arts; do
    arts_rel=""
    for a in $arts; do arts_rel+="${a#"$ROOT"/} "; done
    echo "| $t | $res | $ex | $eps | $cov | $ft | $corp | $rss | ${arts_rel% } |"
  done
} >"$SUM"
cat "$SUM"
fails=$(awk -F'\t' '$2 != "PASS"' "$FUZZ_OUT/results.tsv" | wc -l)
total=$(wc -l <"$FUZZ_OUT/results.tsv")
echo
if [[ $fails == 0 ]]; then
  echo "PASS: $total target すべて crash なし"
else
  echo "FAIL: $fails / $total target で crash・timeout・OOM（fuzz/artifacts/logs/*.log）"
  exit 1
fi
