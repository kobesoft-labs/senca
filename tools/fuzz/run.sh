#!/usr/bin/env bash
# 1 つの fuzz target を決めた時間だけ回す。
#
#   tools/fuzz/run.sh TARGET SECONDS [libFuzzer の追加の引数...]
#
# 種と見つけた入力は fuzz/corpus/TARGET、crash などの再現入力は fuzz/artifacts/TARGET/、
# ログは fuzz/artifacts/logs/TARGET.log。結果は 1 行（タブ区切り）で fuzz/artifacts/results.tsv に足す:
#   target  PASS|FAIL  seconds  execs  exec/s  cov(edges)  ft(features)  corpus  peak_rss_mb  artifacts
# 再現: $FUZZ_BIN_DIR/TARGET fuzz/artifacts/TARGET/crash-...（RUST_BACKTRACE=1 で backtrace）
# 環境変数: SKIP_BUILD=1（build 済み）、FUZZ_TIMEOUT、FUZZ_RSS_MB、FUZZ_SANITIZER（tools/fuzz/common.sh）
set -euo pipefail
source "$(dirname "$0")/common.sh"

T=${1:?usage: run.sh TARGET SECONDS [libfuzzer args...]}
SECS=${2:?usage: run.sh TARGET SECONDS [libfuzzer args...]}
shift 2

if ! fuzz_targets | grep -qx "$T"; then
  echo "run.sh: 知らない target: $T（$(fuzz_targets | tr '\n' ' ')）" >&2
  exit 2
fi
[[ "${SKIP_BUILD:-0}" == 1 ]] || fuzz_build "$T"

BIN=$FUZZ_BIN_DIR/$T
ART=$FUZZ_OUT/$T
LOGDIR=$FUZZ_OUT/logs
mkdir -p "$CORPUS/$T" "$ART" "$LOGDIR"
LOG=$LOGDIR/$T.log
SCRATCH=$(mktemp -d "$CACHE/senca-fuzz-run.XXXXXX")
cleanup() { rm -rf "$SCRATCH"; }
trap cleanup EXIT

touch "$SCRATCH/start"
set +e
# -fork を使わない単一プロセス。最初の crash で止まる（再現入力を残す）
( cd "$SCRATCH" && RUST_BACKTRACE=1 ASAN_OPTIONS=${ASAN_OPTIONS:-detect_leaks=0:symbolize=1} \
  "$BIN" "$CORPUS/$T" \
    -max_total_time="$SECS" -rss_limit_mb="$FUZZ_RSS_MB" -timeout="$FUZZ_TIMEOUT" \
    -artifact_prefix="$ART/" -print_final_stats=1 -close_fd_mask=1 "$@" ) >"$LOG" 2>&1
rc=$?
set -e
arts=$(find "$ART" -maxdepth 1 -type f -newer "$SCRATCH/start" | sort | tr '\n' ' ')

execs=$(sed -n 's/^stat::number_of_executed_units: *//p' "$LOG" | tail -1)
eps=$(sed -n 's/^stat::average_exec_per_sec: *//p' "$LOG" | tail -1)
rss=$(sed -n 's/^stat::peak_rss_mb: *//p' "$LOG" | tail -1)
last=$(grep -E '^#[0-9]+' "$LOG" | tail -1 || true)
cov=$(sed -n 's/.* cov: \([0-9]*\).*/\1/p' <<<"$last")
ft=$(sed -n 's/.* ft: \([0-9]*\).*/\1/p' <<<"$last")
corp=$(sed -n 's/.* corp: \([0-9]*\).*/\1/p' <<<"$last")
if [[ $rc == 0 && -z "$arts" ]]; then res=PASS; else res=FAIL; fi
printf '%s\t%s\t%s\t%s\t%s\t%s\t%s\t%s\t%s\t%s\n' "$T" "$res" "$SECS" "${execs:-?}" "${eps:-?}" "${cov:-?}" "${ft:-?}" \
  "${corp:-?}" "${rss:-?}" "${arts:-}" | tee -a "$FUZZ_OUT/results.tsv"
if [[ $res == FAIL ]]; then
  echo "---- $T: rc=$rc（ログ: $LOG）" >&2
  grep -E 'panicked at|ERROR: libFuzzer|ERROR: AddressSanitizer|SUMMARY|out-of-memory|deadly signal|Test unit written' "$LOG" | head -10 >&2 || true
  exit 1
fi
