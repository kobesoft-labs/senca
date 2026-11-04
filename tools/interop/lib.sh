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
summary() { rm -f "/tmp/senca-ports-$$"; echo "== $PASS passed, $FAIL failed"; [ "$FAIL" = 0 ]; }

# フレームの md5 だけを並べる（時刻は比べない）
frames() { { "$FFMPEG" -v error -i "$1" -map "0:$2" -f framemd5 - 2>"${FRAMES_ERR:-/dev/null}" || true; } | { grep -v "^#" || true; } | awk -F, '{gsub(/ /,"",$6); print $6}'; }

# 空いているポート。
# 試験の待ち受けの番号。カーネルが外向きの接続に使う範囲（ip_local_port_range）の外から選ぶ
# （範囲の中だと、並んで起動するノードの外向きの接続が、まだ bind していない番号を先に取ることがある）。
# 同じ試験の中で同じ番号を 2 度渡さない（$$ は $(port) の中でも呼んだシェルの番号）。
# SENCA_PORT_LO・SENCA_PORT_HI で範囲を狭められる（並べて走らせる試験で番号を分ける）。
port() { python3 - "/tmp/senca-ports-$$" <<'PY'
import os, random, socket, sys
used_file = sys.argv[1]
used = set(open(used_file).read().split()) if os.path.exists(used_file) else set()
lo, hi = (int(x) for x in open("/proc/sys/net/ipv4/ip_local_port_range").read().split())
# SENCA_PORT_MIN・SENCA_PORT_MAX（別名 SENCA_PORT_LO・SENCA_PORT_HI）で範囲を狭められる（同じ機械で並べて試験するとき）
pmin = int(os.environ.get("SENCA_PORT_MIN", os.environ.get("SENCA_PORT_LO", "10000")))
pmax = min(lo - 1, int(os.environ.get("SENCA_PORT_MAX", os.environ.get("SENCA_PORT_HI", str(lo - 1)))))
# 範囲を外向きの接続の範囲の中に明示したとき（どちらも ip_local_port_range の中）は、その範囲を使う
if pmin >= lo and pmax < pmin:
    pmax = int(os.environ.get("SENCA_PORT_MAX", os.environ.get("SENCA_PORT_HI", str(pmin))))
pool = [p for p in range(pmin, pmax + 1) if not 30010 <= p <= 30019 and str(p) not in used]
for _ in range(200):
    p = random.choice(pool)
    try:
        for kind in (socket.SOCK_STREAM, socket.SOCK_DGRAM):
            s = socket.socket(socket.AF_INET, kind); s.bind(("127.0.0.1", p)); s.close()
        open(used_file, "a").write(f"{p}\n")
        print(p); break
    except OSError:
        continue
PY
}

json() { python3 -c "import json,sys;d=json.load(sys.stdin);print(eval('d'+sys.argv[1]))" "$1"; }

# $2 のフレームが $1 の連続した一部と一致し、$3 フレーム以上あること
contiguous() { python3 - "$1" "$2" "$3" <<'PY'
import sys
s=open(sys.argv[1]).read().split(); p=open(sys.argv[2]).read().split()
sys.exit(0 if len(p)>=int(sys.argv[3]) and any(s[i:i+len(p)]==p for i in range(len(s)-len(p)+1)) else 1)
PY
}
