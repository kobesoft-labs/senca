# 相互接続試験の共通部品。FFmpeg は試験の相手としてだけ使う。
set -euo pipefail
FFMPEG=${FFMPEG:-$HOME/tools/ffmpeg}
FFPROBE=${FFPROBE:-$HOME/tools/ffprobe}
# 静的ビルドの FFmpeg は TS を読むときに iconv で落ちることがある（GCONV_PATH を無効にすると避けられる）
export GCONV_PATH=${GCONV_PATH:-/nonexistent}
ROOT=$(cd "$(dirname "${BASH_SOURCE[0]}")/../.." && pwd)
TARGET=${CARGO_TARGET_DIR:-$ROOT/target}
NODE_BIN=${NODE_BIN:-$TARGET/debug/senca-node}
PASS=0; FAIL=0
pass() { echo "PASS $*"; PASS=$((PASS+1)); }
fail() { echo "FAIL $*"; FAIL=$((FAIL+1)); }
summary() { echo "== $PASS passed, $FAIL failed"; [ "$FAIL" = 0 ]; }

# フレームの md5 だけを並べる（時刻は比べない）
frames() { { "$FFMPEG" -v error -i "$1" -map "0:$2" -f framemd5 - 2>"${FRAMES_ERR:-/dev/null}" || true; } | { grep -v "^#" || true; } | awk -F, '{gsub(/ /,"",$6); print $6}'; }

# 空いているポート
port() { python3 -c 'import socket;s=socket.socket();s.bind(("127.0.0.1",0));print(s.getsockname()[1])'; }

json() { python3 -c "import json,sys;d=json.load(sys.stdin);print(eval('d'+sys.argv[1]))" "$1"; }

# $2 のフレームが $1 の連続した一部と一致し、$3 フレーム以上あること
contiguous() { python3 - "$1" "$2" "$3" <<'PY'
import sys
s=open(sys.argv[1]).read().split(); p=open(sys.argv[2]).read().split()
sys.exit(0 if len(p)>=int(sys.argv[3]) and any(s[i:i+len(p)]==p for i in range(len(s)-len(p)+1)) else 1)
PY
}
