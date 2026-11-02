#!/usr/bin/env bash
# キャプチャ機器（senca-capture、REQ-004）の相互接続試験。
#
# 1. capture worker の `list`: 試験用の機器が出る、DeckLink はライブラリが無ければ「使えない」と理由を出す、
#    実機で試していないバックエンド（V4L2・ALSA・DeckLink）は「unverified」と表示される。
# 2. capture worker の `serve`: サンドボックス（no_new_privs・Landlock・seccomp）を掛けて Hello を送り、
#    標準入力が閉じたら終了コード 0 で終わる。
# 3. senca-capture の試験（単体 + 通し）: 試験用の機器 → YUYV/UYVY/NV12/MJPEG → I420 → VP9/AV1/VP8 + Opus →
#    fMP4（senca-fmp4）→ FFmpeg で誤りなく復号でき、フレーム数が合う。worker のプロセス越しでも同じ。
#    H.264 はハードウェアが無ければ CAPACITY_UNAVAILABLE。
# 4. 実機: /dev/video* があれば一覧に出ることを確かめる。無ければ V4L2 は「未検証」と表示する
#    （v4l2loopback の読み込みには root 権限が要る）。ALSA・DeckLink も実機が無ければ「未検証」。
#
# 符号化のライブラリは SENCA_CODEC_LIB_DIR（既定 ~/tools/codeclibs/usr/lib/x86_64-linux-gnu）。
# FFmpeg（~/tools/ffmpeg・~/tools/ffprobe）は試験の相手としてだけ使う。
set -euo pipefail

ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/../.." && pwd)"
CARGO="${CARGO:-$HOME/.cargo/bin/cargo}"
PYTHON="${PYTHON:-python3}"
export CARGO_TARGET_DIR="${CARGO_TARGET_DIR:-$HOME/.cache/senca-target-capture}"
export SENCA_CODEC_LIB_DIR="${SENCA_CODEC_LIB_DIR:-$HOME/tools/codeclibs/usr/lib/x86_64-linux-gnu}"
export FFMPEG="${FFMPEG:-$HOME/tools/ffmpeg}"
export FFPROBE="${FFPROBE:-$HOME/tools/ffprobe}"

mkdir -p "$HOME/.cache"
WORK="$(mktemp -d "$HOME/.cache/senca-capture-interop.XXXXXX")"
cleanup() { rm -rf "$WORK"; }
trap cleanup EXIT

PASS=0
FAIL=0
pass() { echo "PASS: $1"; PASS=$((PASS + 1)); }
fail() { echo "FAIL: $1"; FAIL=$((FAIL + 1)); }
unverified() { echo "UNVERIFIED: $1"; }

echo "== build"
(cd "$ROOT" && "$CARGO" build -q -p senca-capture --bins)
WORKER="$CARGO_TARGET_DIR/debug/senca-capture-worker"

echo "== list"
if "$WORKER" list --synthetic >"$WORK/list.json" 2>"$WORK/list.err"; then
    pass "worker list exits 0"
else
    fail "worker list ($(cat "$WORK/list.err"))"
fi
# 一覧の検査（隔離モードの Python、ファイルは引数で渡す）
cat >"$WORK/check_list.py" <<'EOF'
import json, sys
d = json.load(open(sys.argv[1]))
mode = sys.argv[2]
backends = {b["backend"]: b for b in d["backends"]}
devices = d["devices"]
if mode == "synthetic":
    sys.exit(0 if any(x["id"] == "synthetic:0" and x["verification"] == "verified" for x in devices) else 1)
if mode == "unverified":
    bad = [b for b in d["backends"] if b["backend"] != "synthetic" and b["verification"] != "unverified"]
    bad += [x for x in devices if x["backend"] != "synthetic" and x["verification"] != "unverified"]
    sys.exit(0 if not bad else 1)
if mode == "decklink":
    b = backends.get("decklink")
    if b is None:
        sys.exit(1)
    print(f"decklink: available={b['available']} detail={b['detail']}")
    sys.exit(0 if b["available"] or "libDeckLinkAPI.so" in b["detail"] else 1)
if mode == "v4l2-count":
    print(sum(1 for x in devices if x["backend"] == "v4l2"))
    sys.exit(0)
if mode == "alsa":
    b = backends.get("alsa")
    print(f"alsa: available={b['available']} detail={b['detail']}")
    sys.exit(0)
sys.exit(2)
EOF
chk() { "$PYTHON" -I "$WORK/check_list.py" "$WORK/list.json" "$1"; }
if chk synthetic; then pass "synthetic device listed (verified)"; else fail "synthetic device missing"; fi
if chk unverified; then pass "hardware backends are marked unverified"; else fail "hardware backend not marked unverified"; fi
if chk decklink; then pass "DeckLink probe reports cleanly"; else fail "DeckLink probe"; fi
chk alsa || true

echo "== serve (sandbox, hello, clean exit)"
set +e
"$WORKER" serve </dev/null >"$WORK/hello.bin" 2>"$WORK/serve.err"
code=$?
set -e
if [ "$code" = 0 ]; then pass "worker serve exits 0 when stdin closes"; else fail "worker serve exit code $code"; fi
# 最初のフレームの種類が Hello（1）
if [ -s "$WORK/hello.bin" ] && [ "$(od -An -tu1 -j4 -N1 "$WORK/hello.bin" | tr -d ' ')" = 1 ]; then
    pass "worker sends Hello first"
else
    fail "no Hello frame"
fi
if grep -q "no_new_privs" "$WORK/serve.err" && grep -q "seccomp deny-list" "$WORK/serve.err"; then
    pass "sandbox applied ($(grep -o 'sandbox: .*' "$WORK/serve.err" | head -1))"
else
    fail "sandbox not applied: $(cat "$WORK/serve.err")"
fi

echo "== tests (unit + end-to-end through FFmpeg)"
if (cd "$ROOT" && "$CARGO" test -q -p senca-capture -- --test-threads=2) >"$WORK/test.log" 2>&1; then
    pass "cargo test -p senca-capture ($(grep -E '^test result' "$WORK/test.log" | awk '{s+=$4} END {print s}') tests)"
    if grep -q "SKIP" "$WORK/test.log"; then
        echo "note: some tests were skipped:"
        grep "SKIP" "$WORK/test.log" | sort -u
    fi
else
    tail -50 "$WORK/test.log"
    fail "cargo test -p senca-capture"
fi
# FFmpeg による復号の確認が実際に走ったか（--nocapture で通しの試験だけ）
if (cd "$ROOT" && "$CARGO" test -q -p senca-capture --test e2e -- --nocapture --test-threads=1) >"$WORK/e2e.log" 2>&1; then
    n=$(grep -c "ffmpeg decoded" "$WORK/e2e.log" || true)
    if [ "$n" -ge 3 ]; then
        pass "FFmpeg decoded $n captured fMP4 files without errors"
        grep "ffmpeg decoded" "$WORK/e2e.log" | sed 's/^.*ffmpeg decoded/  ffmpeg decoded/'
    else
        fail "FFmpeg checks did not run (libraries or FFmpeg missing?)"
    fi
else
    tail -50 "$WORK/e2e.log"
    fail "end-to-end tests"
fi

echo "== real hardware"
shopt -s nullglob
videos=(/dev/video*)
if [ "${#videos[@]}" -gt 0 ]; then
    n=$(chk v4l2-count)
    if [ "$n" -gt 0 ]; then
        pass "V4L2 lists $n capture device(s) (frame capture still to be checked by hand)"
    else
        fail "V4L2 nodes exist (${videos[*]}) but none listed"
    fi
else
    unverified "V4L2 (no /dev/video*; v4l2loopback needs root) - tested with a mock V4L2 device only"
fi
if [ -e /proc/asound/pcm ] && grep -q capture /proc/asound/pcm; then
    echo "note: ALSA capture PCM present; capture not exercised by this script"
fi
unverified "ALSA capture (no libasound.so.2 / capture PCM on this machine)"
unverified "Blackmagic DeckLink (no board / Desktop Video driver on this machine)"

echo "== $PASS passed, $FAIL failed"
[ "$FAIL" = 0 ]
