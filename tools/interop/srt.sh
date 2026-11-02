#!/usr/bin/env bash
# SRT の相互接続試験（FFmpeg に内蔵の libsrt を相手にする）。
#
#  A. FFmpeg（caller）→ senca の listener（examples/srt_listen）
#     平文・Stream ID・pbkeylen 16/24/32。受信した TS が FFmpeg 自身の -c copy 出力のバイト前置部で、
#     framemd5 がストリームごとに元の前置部になっていること（libsrt の live mode は閉じるときに
#     未送信の末尾を捨てるため、末尾の欠けは許す）。
#  B. senca の caller（examples/srt_send）→ FFmpeg（listener）
#     平文・pbkeylen 16/24/32・鍵の更新（km-refresh）。framemd5 が元と完全に一致すること。
#  C. 拒否：誤ったパスフレーズ（両方向）、片側だけ暗号（平文へ降格しない）、Stream ID（1403）。
#  D. 損失を入れる中継（examples/udp_relay、2% 破棄・50 ms 遅延）を挟んだ A と B。
#     再送で埋まり、A は前置部一致・破棄 0、B は framemd5 完全一致。
#
#  E. （任意）AES-GCM。FFmpeg の libsrt は 1.4 系で GCM を持たないため、SRT_GCM_IMAGE に
#     ENABLE_AEAD_API_PREVIEW を付けて作った libsrt 1.5.4 の srt-live-transmit を含む Docker イメージを
#     指定したときだけ行う（損失中継を挟み、両方向）。イメージの作り方の例:
#       FROM debian:trixie-slim
#       RUN apt-get update && apt-get install -y build-essential cmake libssl-dev pkg-config tclsh curl ca-certificates
#       RUN curl -sL https://github.com/Haivision/srt/archive/refs/tags/v1.5.4.tar.gz | tar xz -C /opt \
#           && cd /opt/srt-1.5.4 && cmake -S . -B build -DENABLE_AEAD_API_PREVIEW=ON -DUSE_ENCLIB=openssl-evp \
#           && cmake --build build -j8 && cmake --install build && ldconfig
#
# 環境変数: FFMPEG（既定 ~/tools/ffmpeg）、CARGO（既定 cargo）、CARGO_TARGET_DIR、
#           SRT_BIN_DIR（指定すればビルドせず、そこにある examples を使う）、SRT_GCM_IMAGE。
# ポートは 44000〜44999 を使う。
set -euo pipefail

source "$(dirname "$0")/lib.sh"
CARGO=${CARGO:-cargo}
# 静的ビルドの FFmpeg は MPEG-TS の SDT を読むときに iconv（glibc の gconv を dlopen）で落ちることが
# あるため、gconv を読ませない。
export GCONV_PATH=${GCONV_PATH:-/nonexistent}

WORK=$(mktemp -d)
PIDS=()
cleanup() {
    for p in "${PIDS[@]}"; do kill "$p" 2>/dev/null || true; done
    rm -rf "$WORK"
}
trap cleanup EXIT

if [[ -z "${SRT_BIN_DIR:-}" ]]; then
    (cd "$ROOT" && "$CARGO" build -q -p senca-srt --examples)
    SRT_BIN_DIR="$TARGET/debug/examples"
fi
LISTEN="$SRT_BIN_DIR/srt_listen"
SEND="$SRT_BIN_DIR/srt_send"
RELAY="$SRT_BIN_DIR/udp_relay"
for b in "$LISTEN" "$SEND" "$RELAY"; do
    [[ -x "$b" ]] || { echo "FAIL: $b not built"; exit 1; }
done

NEXT_PORT=$((44000 + (RANDOM % 90) * 10))
alloc_port() { NEXT_PORT=$((NEXT_PORT + 1)); PORT=$NEXT_PORT; }

# 4 秒・約 1 Mbit/s の H.264 + AAC
SRC="$WORK/src.ts"
"$FFMPEG" -nostdin -v error -y -f lavfi -i testsrc2=size=640x360:rate=25 \
    -f lavfi -i sine=frequency=440:sample_rate=48000 -t 4 \
    -c:v libx264 -b:v 1000k -g 25 -bf 2 -pix_fmt yuv420p -c:a aac -ac 2 -f mpegts "$SRC"
# FFmpeg 自身の -c copy 出力（caller として送る TS と同じバイト列になる）
REF="$WORK/ref.ts"
"$FFMPEG" -nostdin -v error -y -i "$SRC" -c copy -f mpegts "$REF"

fmd5() { "$FFMPEG" -nostdin -v error -i "$1" -f framemd5 - 2>/dev/null | grep -v '^#' | awk -F, '{gsub(/ /,"",$1); gsub(/ /,"",$6); print $1, $6}'; }
fmd5 "$SRC" >"$WORK/src.md5"

# ストリームごとに out が src の前置部（末尾の数フレームを除く）で、各ストリームの min_ratio 以上を含むか
prefix_ok() { # src.md5 out.md5 min_ratio
    python3 - "$1" "$2" "$3" <<'EOF'
import sys, collections
def load(p):
    d = collections.defaultdict(list)
    for line in open(p):
        s, m = line.split()
        d[s].append(m)
    return d
a, b, r = load(sys.argv[1]), load(sys.argv[2]), float(sys.argv[3])
ok = set(a) == set(b)
for s in a:
    x, y = a[s], b.get(s, [])
    # 末尾が切れると、最後の PES とその参照（B フレームの並べ替え）から復号した数フレームは違ってよい
    # （受信バイト列そのものの一致は byte_prefix で確かめている）
    k = max(len(y) - 3, 0)
    if y[:k] != x[:k] or len(y) < r * len(x):
        ok = False
    print(f"  stream {s}: {len(y)}/{len(x)} frames", file=sys.stderr)
sys.exit(0 if ok else 1)
EOF
}

# 先頭がバイト単位で一致するか（out が ref の前置部）
byte_prefix() { # ref out
    local n
    n=$(stat -c %s "$2")
    [[ "$n" -gt 0 ]] && cmp -s -n "$n" "$1" "$2"
}

wait_line() { # file pattern
    for _ in $(seq 100); do
        grep -q "$2" "$1" 2>/dev/null && return 0
        sleep 0.05
    done
    return 1
}

wait_udp() { # port
    for _ in $(seq 100); do
        ss -lun 2>/dev/null | grep -q "127.0.0.1:$1 " && return 0
        sleep 0.05
    done
    return 1
}

# A: FFmpeg caller → senca listener。出力: $WORK/$name.{ts,log,ff}
run_a() { # name "listener args" "srt query" [relay args]
    local name=$1 largs=$2 query=$3 relay=${4:-}
    alloc_port
    local lp=$PORT target=$PORT
    # shellcheck disable=SC2086
    "$LISTEN" --port "$lp" --out "$WORK/$name.ts" --timeout 4 $largs >"$WORK/$name.log" 2>&1 &
    local lpid=$!
    PIDS+=("$lpid")
    wait_line "$WORK/$name.log" LISTENING || { fail "$name: listener did not start"; return 1; }
    if [[ -n "$relay" ]]; then
        alloc_port
        target=$PORT
        # shellcheck disable=SC2086
        "$RELAY" --listen "$target" --to "127.0.0.1:$lp" --idle 3 $relay >"$WORK/$name.relay" 2>&1 &
        PIDS+=($!)
        wait_line "$WORK/$name.relay" RELAY || { fail "$name: relay did not start"; return 1; }
    fi
    timeout 60 "$FFMPEG" -nostdin -v error -re -i "$SRC" -c copy -f mpegts \
        "srt://127.0.0.1:$target?mode=caller&$query" >"$WORK/$name.ff" 2>&1 || true
    wait "$lpid" || true
}

check_a_ok() { # name expected-encryption min_ratio
    local name=$1 enc=$2 ratio=$3 res
    res=$(grep RESULT "$WORK/$name.log" || true)
    if ! grep -q "connected=true" <<<"$res"; then
        fail "$name: not connected: $(tail -3 "$WORK/$name.log" "$WORK/$name.ff" | tr '\n' ' ')"
        return
    fi
    if ! grep -q "encryption=$enc " <<<"$res"; then
        fail "$name: encryption mismatch: $res"
        return
    fi
    if ! grep -q "dropped=0 undecrypted=0" <<<"$res"; then
        fail "$name: drops: $res"
        return
    fi
    if ! byte_prefix "$REF" "$WORK/$name.ts"; then
        fail "$name: received TS is not a byte prefix of FFmpeg's own remux"
        return
    fi
    fmd5 "$WORK/$name.ts" >"$WORK/$name.md5"
    if prefix_ok "$WORK/src.md5" "$WORK/$name.md5" "$ratio" 2>"$WORK/$name.cmp"; then
        pass "$name ($(stat -c %s "$WORK/$name.ts")/$(stat -c %s "$REF") bytes; $(grep -o 'loss=[0-9]* retrans_recv=[0-9]*' <<<"$res"))"
    else
        fail "$name: framemd5 mismatch: $(tr '\n' ' ' <"$WORK/$name.cmp")"
    fi
}

check_a_rejected() { # name reason-code
    local name=$1 code=$2
    if grep -q "connected=false" "$WORK/$name.log" && grep -q "^REJECT .* $code " "$WORK/$name.log"; then
        pass "$name (rejected $code)"
    else
        fail "$name: expected rejection $code: $(grep -E 'RESULT|REJECT' "$WORK/$name.log" | tr '\n' ' ')"
    fi
}

# B: senca caller → FFmpeg listener。出力: $WORK/$name.{ts,send,ff}
run_b() { # name "srt query" "sender args" [relay args]
    local name=$1 query=$2 sargs=$3 relay=${4:-}
    alloc_port
    local fp=$PORT target=$PORT
    timeout 60 "$FFMPEG" -nostdin -v warning -y -i "srt://127.0.0.1:$fp?mode=listener&$query" \
        -c copy -f mpegts "$WORK/$name.ts" >"$WORK/$name.ff" 2>&1 &
    local fpid=$!
    PIDS+=("$fpid")
    wait_udp "$fp" || { fail "$name: ffmpeg did not listen"; return 1; }
    if [[ -n "$relay" ]]; then
        alloc_port
        target=$PORT
        # shellcheck disable=SC2086
        "$RELAY" --listen "$target" --to "127.0.0.1:$fp" --idle 3 $relay >"$WORK/$name.relay" 2>&1 &
        PIDS+=($!)
        wait_line "$WORK/$name.relay" RELAY || { fail "$name: relay did not start"; return 1; }
    fi
    # shellcheck disable=SC2086
    "$SEND" --to "127.0.0.1:$target" --in "$SRC" --duration 4 $sargs >"$WORK/$name.send" 2>&1 || true
    if grep -q "close=Rejected" "$WORK/$name.send"; then
        kill "$fpid" 2>/dev/null || true
    fi
    wait "$fpid" || true
}

check_b_ok() { # name
    local name=$1 res
    res=$(grep RESULT "$WORK/$name.send" || true)
    if ! grep -q "close=Local" <<<"$res"; then
        fail "$name: sender did not finish: $(tail -3 "$WORK/$name.send" | tr '\n' ' ')"
        return
    fi
    fmd5 "$WORK/$name.ts" >"$WORK/$name.md5"
    if cmp -s "$WORK/src.md5" "$WORK/$name.md5"; then
        pass "$name ($(wc -l <"$WORK/$name.md5") frames; $(grep -o 'retrans=[0-9]* snd_loss=[0-9]*' <<<"$res"))"
    else
        fail "$name: framemd5 differs ($(wc -l <"$WORK/$name.md5")/$(wc -l <"$WORK/src.md5") frames)"
    fi
}

check_b_rejected() { # name reason-code
    local name=$1 code=$2
    if grep -q "close=Rejected(RejectReason($code))" "$WORK/$name.send"; then
        pass "$name (rejected by libsrt: $code)"
    else
        fail "$name: expected rejection $code: $(grep RESULT "$WORK/$name.send" || tail -2 "$WORK/$name.send")"
    fi
}

LAT="latency=200000"

echo "== A: FFmpeg caller -> senca listener"
run_a a_plain_sid "--resource live/cam1 --latency 200" "$LAT&streamid=#!::r=live/cam1,m=publish"
check_a_ok a_plain_sid None 0.85
for k in 16 24 32; do
    run_a "a_aes$k" "--passphrase interop-secret-$k --latency 200" "$LAT&passphrase=interop-secret-$k&pbkeylen=$k"
    check_a_ok "a_aes$k" "Some((AesCtr, Aes$((k * 8))))" 0.85
done

echo "== B: senca caller -> FFmpeg listener"
run_b b_plain "$LAT" "--latency 200"
check_b_ok b_plain
for k in 16 24 32; do
    run_b "b_aes$k" "$LAT&passphrase=interop-secret-$k&pbkeylen=$k" "--latency 200 --passphrase interop-secret-$k --pbkeylen $k"
    check_b_ok "b_aes$k"
done
run_b b_km_refresh "$LAT&passphrase=interop-rotate-1" "--latency 200 --passphrase interop-rotate-1 --km-refresh 100"
check_b_ok b_km_refresh

echo "== C: rejections"
run_a a_wrong_pass "--passphrase right-secret-1" "$LAT&passphrase=wrong-secret-1"
check_a_rejected a_wrong_pass 10
run_a a_unsecure "--passphrase right-secret-1" "$LAT"
check_a_rejected a_unsecure 11
run_a a_plain_vs_enc "" "$LAT&passphrase=client-secret-1"
check_a_rejected a_plain_vs_enc 11
run_a a_forbidden "--resource live/cam1" "$LAT&streamid=#!::r=live/other,m=publish"
check_a_rejected a_forbidden 1403
run_b b_wrong_pass "$LAT&passphrase=right-secret-1" "--passphrase wrong-secret-1"
check_b_rejected b_wrong_pass 10
run_b b_unsecure "$LAT" "--passphrase client-secret-1"
check_b_rejected b_unsecure 11

echo "== D: lossy relay (2% drop, 50 ms delay)"
run_a a_lossy "--passphrase lossy-secret-1 --latency 400" "latency=400000&passphrase=lossy-secret-1" "--loss 2 --delay 50 --seed 7"
check_a_ok a_lossy "Some((AesCtr, Aes128))" 0.85
if grep -q "retrans_recv=0 " "$WORK/a_lossy.log"; then fail "a_lossy: no retransmission observed"; fi
run_b b_lossy "latency=400000&passphrase=lossy-secret-2&pbkeylen=32" "--latency 400 --passphrase lossy-secret-2 --pbkeylen 32" "--loss 2 --delay 50 --seed 9"
check_b_ok b_lossy
if grep -q "retrans=0 " "$WORK/b_lossy.send"; then fail "b_lossy: no retransmission observed"; fi

echo "== E: AES-GCM with libsrt 1.5.4 (optional)"
if [[ -z "${SRT_GCM_IMAGE:-}" ]]; then
    echo "SKIP (SRT_GCM_IMAGE not set)"
else
    # senca caller（GCM・AES-256・鍵の更新）→ srt-live-transmit listener、3% 破棄
    alloc_port; gp=$PORT; alloc_port; gr=$PORT
    timeout 60 docker run --rm --network host -v "$WORK:/w" "$SRT_GCM_IMAGE" sh -c \
        "srt-live-transmit -q -a:no 'srt://127.0.0.1:$gp?mode=listener&passphrase=gcm-secret-123&cryptomode=2&latency=400' file://con > /w/e_gcm_rx.ts" \
        >"$WORK/e1.docker" 2>&1 &
    dpid=$!
    PIDS+=("$dpid")
    wait_udp "$gp" || sleep 3
    "$RELAY" --listen "$gr" --to "127.0.0.1:$gp" --idle 3 --loss 3 --delay 50 --seed 3 >"$WORK/e1.relay" 2>&1 &
    PIDS+=($!)
    wait_line "$WORK/e1.relay" RELAY || true
    "$SEND" --to "127.0.0.1:$gr" --in "$SRC" --duration 4 --latency 400 --passphrase gcm-secret-123 --gcm \
        --pbkeylen 32 --km-refresh 100 >"$WORK/e1.send" 2>&1 || true
    wait "$dpid" || true
    if grep -q "encryption: Some((AesGcm, Aes256))" "$WORK/e1.send" && cmp -s "$SRC" "$WORK/e_gcm_rx.ts"; then
        pass "e_gcm_caller ($(grep -o 'retrans=[0-9]*' "$WORK/e1.send"))"
    else
        fail "e_gcm_caller: $(grep RESULT "$WORK/e1.send" || true) $(tail -2 "$WORK/e1.docker" | tr '\n' ' ')"
    fi
    # srt-live-transmit caller（GCM・AES-192）→ senca listener、3% 破棄
    alloc_port; lp=$PORT; alloc_port; gr=$PORT
    "$LISTEN" --port "$lp" --out "$WORK/e_gcm_tx.ts" --timeout 4 --latency 400 --passphrase gcm-secret-456 \
        >"$WORK/e2.log" 2>&1 &
    lpid=$!
    PIDS+=("$lpid")
    wait_line "$WORK/e2.log" LISTENING || true
    "$RELAY" --listen "$gr" --to "127.0.0.1:$lp" --idle 3 --loss 3 --delay 50 --seed 5 >"$WORK/e2.relay" 2>&1 &
    PIDS+=($!)
    wait_line "$WORK/e2.relay" RELAY || true
    # 標準入力から実時間で流す（srt-live-transmit は接続前に読んだ分を捨てる）
    python3 -c '
import sys, time
d = open(sys.argv[1], "rb").read(); n = 1316; t0 = time.time(); k = len(d) // n + 1
for i in range(0, len(d), n):
    sys.stdout.buffer.write(d[i:i + n]); sys.stdout.buffer.flush()
    time.sleep(max(0, t0 + (i / n) * 4 / k - time.time()))
time.sleep(1.5)
' "$SRC" | timeout 60 docker run -i --rm --network host "$SRT_GCM_IMAGE" srt-live-transmit -q -a:no -chunk:1316 \
        file://con "srt://127.0.0.1:$gr?mode=caller&passphrase=gcm-secret-456&cryptomode=2&latency=400&pbkeylen=24" \
        >"$WORK/e2.docker" 2>&1 || true
    wait "$lpid" || true
    # 受け取ったものが元の連続した区間で、半分以上あること（srt-live-transmit は接続前に読んだ分と、
    # 入力の終わりで未送信の分を捨てる）
    if grep -q "encryption=Some((AesGcm, Aes192)) .*dropped=0 undecrypted=0" "$WORK/e2.log" &&
        python3 -c '
import sys
a = open(sys.argv[1], "rb").read(); b = open(sys.argv[2], "rb").read()
sys.exit(0 if len(b) >= 0.5 * len(a) and a.find(b) >= 0 else 1)' "$SRC" "$WORK/e_gcm_tx.ts"; then
        pass "e_gcm_listener ($(grep -o 'loss=[0-9]* retrans_recv=[0-9]*' "$WORK/e2.log"))"
    else
        fail "e_gcm_listener: $(grep RESULT "$WORK/e2.log" || true)"
    fi
fi

summary
