#!/usr/bin/env bash
# RTP・RTSP の相互接続試験（FFmpeg・mediamtx を相手にする。どちらも試験の相手としてだけ使う）。
#
#  C1. FFmpeg の `-f rtp`（VP8・VP9・H.264・H.265・Opus・AAC mpeg4-generic・AAC LATM）→ senca の rtp_recv
#      （FFmpeg の `-sdp_file` を senca-sdp で読む）→ 復号結果（映像）・パケット（音声）が元と一致
#  C2. senca の rtp_send（senca-sdp で SDP を作る）→ FFmpeg が SDP で受信 → 一致
#      （AV1 は FFmpeg 7.0 に RTP の実装がないので senca 同士で送受し、FFmpeg の復号結果で比べる）
#  A.  FFmpeg が mediamtx へ publish（H.264+AAC、H.265+Opus）→ senca の rtsp_pull が UDP・TCP で pull →
#      受け取ったフレーム・パケットが元の連続した一部と一致（途中から参加するのでキーフレームから）
#  D.  mediamtx の Digest 認証: 資格情報ありで pull できる・なし／誤りでは失敗する
#  B.  senca の rtsp_serve（TS の AU を senca-rtp で packetize）→ FFmpeg が RTSP（TCP・UDP・Digest）で受信 → 一致
#
# ポートは 45000〜45999 を使う。mediamtx は `docker run --network host` で起動し、終わったら止める。
# FFmpeg 7.0.2（静的ビルド）は MPEG-TS の読み込みで落ちるので、元は MKV にし、senca の入力の TS は FFmpeg で書く。
#
# 環境変数: FFMPEG（既定 ~/tools/ffmpeg）、CARGO（既定 cargo）、MEDIAMTX_IMAGE（既定 bluenviron/mediamtx:latest）、
#           RTP_BIN_DIR（指定すればビルドせず、そこの examples を使う）。
set -euo pipefail

FFMPEG=${FFMPEG:-$HOME/tools/ffmpeg}
CARGO=${CARGO:-cargo}
IMAGE=${MEDIAMTX_IMAGE:-bluenviron/mediamtx:latest}
ROOT=$(cd "$(dirname "$0")/../.." && pwd)

WORK=$(mktemp -d)
PIDS=()
CONTAINERS=()
cleanup() {
    for p in "${PIDS[@]}"; do kill "$p" 2>/dev/null || true; done
    for c in "${CONTAINERS[@]}"; do docker stop -t 2 "$c" >/dev/null 2>&1 || true; done
    rm -rf "$WORK"
}
trap cleanup EXIT

PASS=0
FAIL=0
pass() { echo "PASS $*"; PASS=$((PASS + 1)); }
fail() { echo "FAIL $*"; FAIL=$((FAIL + 1)); }

if [[ -z "${RTP_BIN_DIR:-}" ]]; then
    (cd "$ROOT" && "$CARGO" build -q -p senca-rtsp --examples)
    RTP_BIN_DIR="${CARGO_TARGET_DIR:-$ROOT/target}/debug/examples"
fi
for b in rtp_recv rtp_send rtsp_pull rtsp_serve; do
    [[ -x "$RTP_BIN_DIR/$b" ]] || { echo "missing $RTP_BIN_DIR/$b"; exit 1; }
done
RECV="$RTP_BIN_DIR/rtp_recv"
SEND="$RTP_BIN_DIR/rtp_send"
PULL="$RTP_BIN_DIR/rtsp_pull"
SERVE="$RTP_BIN_DIR/rtsp_serve"

# 45000〜45999 の中で、実行ごとに少しずらす（偶数から）
BASE=$((45000 + (RANDOM % 40) * 20))
NEXT=$BASE
alloc() { # 連続した 10 個を確保して先頭を返す
    local p=$NEXT
    NEXT=$((NEXT + 10))
    [[ $NEXT -le 45999 ]] || { echo "port range exhausted"; exit 1; }
    echo "$p"
}

FF=("$FFMPEG" -hide_banner -loglevel error -nostdin -y)
cd "$WORK"

cat >compare.py <<'EOF'
import sys
mode, a, b = sys.argv[1], sys.argv[2], sys.argv[3]
src = [l.strip() for l in open(a) if l.strip()]
got = [l.strip() for l in open(b) if l.strip()]
arg = int(sys.argv[4]) if len(sys.argv) > 4 else 0
if mode == "exact":
    ok = src == got and len(src) > 0
elif mode == "prefix":
    ok = len(got) >= len(src) - arg and got == src[: len(got)] and len(got) > 0
else:  # run: got が src の連続した一部で、長さが arg 以上
    ok = False
    if len(got) >= arg and len(got) > 0:
        for i in range(len(src) - len(got) + 1):
            if src[i : i + len(got)] == got:
                ok = True
                break
print(f"src={len(src)} got={len(got)}")
sys.exit(0 if ok else 1)
EOF

# 復号結果（映像）またはパケット（音声、-c copy）の md5 を 1 行ずつ
vmd5() { "${FF[@]}" -i "$1" -map "0:${2:-v:0}" -fps_mode passthrough -f framemd5 - | grep -v '^#' | awk -F, '{gsub(/ /,"",$6); print $6}'; }
amd5() { "${FF[@]}" -i "$1" -map "0:${2:-a:0}" -c copy -f framemd5 - | grep -v '^#' | awk -F, '{gsub(/ /,"",$6); print $6}'; }
check() { # name mode src got [arg]
    local out
    if out=$(python3 -I compare.py "$2" "$3" "$4" "${5:-0}"); then pass "$1 ($out)"; else fail "$1 ($out)"; fi
}

echo "== sources"
V=(-f lavfi -i testsrc2=size=320x240:rate=30)
A=(-f lavfi -i sine=frequency=440:sample_rate=48000)
"${FF[@]}" "${V[@]}" -t 3 -c:v libvpx -b:v 500k -g 30 vp8.ivf
"${FF[@]}" "${V[@]}" -t 3 -c:v libvpx-vp9 -b:v 500k -g 30 vp9.ivf
"${FF[@]}" "${V[@]}" -t 3 -c:v libaom-av1 -cpu-used 8 -b:v 300k -g 30 av1.ivf
"${FF[@]}" "${V[@]}" -t 3 -c:v libx264 -g 30 -bf 2 h264.mkv
"${FF[@]}" "${V[@]}" -t 3 -c:v libx265 -x265-params log-level=none:keyint=30 h265.mkv
"${FF[@]}" "${A[@]}" -t 3 -c:a libopus -b:a 64k opus.mkv
"${FF[@]}" "${A[@]}" -t 3 -c:a aac -b:a 96k aac.mkv
"${FF[@]}" "${V[@]}" "${A[@]}" -t 15 -c:v libx264 -g 30 -bf 2 -c:a aac -b:a 96k av264.mkv
"${FF[@]}" "${V[@]}" "${A[@]}" -t 15 -c:v libx265 -x265-params log-level=none:keyint=30 -c:a libopus -b:a 64k av265.mkv
"${FF[@]}" "${V[@]}" "${A[@]}" -t 4 -c:v libx264 -g 30 -bf 2 -c:a aac -b:a 96k short264.mkv
"${FF[@]}" "${V[@]}" "${A[@]}" -t 4 -c:v libx265 -x265-params log-level=none:keyint=30 -c:a libopus -b:a 64k short265.mkv
for f in h264 h265 opus aac short264 short265; do "${FF[@]}" -i $f.mkv -map 0 -c copy -f mpegts $f.ts; done
for f in vp8 vp9 av1; do vmd5 $f.ivf >$f.src; done
for f in h264 h265; do vmd5 $f.mkv >$f.src; done
for f in opus aac; do amd5 $f.mkv >$f.src; done
for f in av264 av265 short264 short265; do vmd5 $f.mkv >$f.v.src; amd5 $f.mkv >$f.a.src; done

echo "== C1. FFmpeg -f rtp → senca rtp_recv"
# name input map kind(v|a) compare-mode src [FFmpeg の追加オプション]
_c1() {
    local name=$1 in=$2 map=$3 kind=$4 mode=$5 src=$6
    shift 6
    local p
    p=$(alloc)
    rm -f "$name.sdp"
    timeout 60 "$RECV" "$p" "$name.sdp" "rx_$name" 2 >"rx_$name.log" 2>&1 &
    local rp=$!
    sleep 0.3
    timeout 30 "${FF[@]}" -re -i "$in" -map "$map" -c copy -strict experimental "$@" -f rtp -sdp_file "$name.sdp" "rtp://127.0.0.1:$p" || true
    wait $rp || true
    local out
    out=$(awk '/^TRACK/ {print $4}' "rx_$name.log")
    if [[ -z "$out" || ! -s "$out" ]]; then fail "C1 $name (no output: $(tail -1 "rx_$name.log"))"; return; fi
    local got=$out
    if [[ $kind == v ]]; then
        got="rx_$name.dec.md5"
        vmd5 "$out" >"$got"
    fi
    check "C1 $name" "$mode" "$src" "$got" 5
}
_c1 vp8 vp8.ivf 0:v v exact vp8.src
_c1 vp9 vp9.ivf 0:v v exact vp9.src
_c1 h264 h264.mkv 0:v v exact h264.src
_c1 h265 h265.mkv 0:v v exact h265.src
_c1 opus opus.mkv 0:a a exact opus.src
# FFmpeg の mpeg4-generic は最後にまとめた AU を送らないことがあるので、先頭からの一致（5 個まで欠け可）で見る
_c1 aac aac.mkv 0:a a prefix aac.src
_c1 latm aac.mkv 0:a a exact aac.src -rtpflags latm

echo "== C2. senca rtp_send → FFmpeg"
c2() { # name input kind src
    local name=$1 in=$2 kind=$3 src=$4
    local p opt=()
    p=$(alloc)
    [[ $kind == a ]] && opt=(-c copy)
    "$SEND" sdp "$in" 0 "$p" "tx_$name.sdp"
    timeout 60 "${FF[@]}" -protocol_whitelist file,udp,rtp -i "tx_$name.sdp" -map 0 "${opt[@]}" -fps_mode passthrough -f framemd5 "tx_$name.out" &
    local fp=$!
    sleep 1.5
    "$SEND" send "$in" 0 "$p" >/dev/null
    wait $fp || true
    grep -v '^#' "tx_$name.out" | awk -F, '{gsub(/ /,"",$6); print $6}' >"tx_$name.md5" || true
    check "C2 $name" exact "$src" "tx_$name.md5"
}
c2 h264 h264.ts v h264.src
c2 h265 h265.ts v h265.src
c2 vp8 vp8.ivf v vp8.src
c2 vp9 vp9.ivf v vp9.src
c2 opus opus.ts a opus.src
c2 aac aac.ts a aac.src
# AV1: senca → senca（FFmpeg 7.0 は AV1 の RTP を扱えない）
p=$(alloc)
"$SEND" sdp av1.ivf 0 "$p" av1_loop.sdp
timeout 60 "$RECV" "$p" av1_loop.sdp rx_av1 2 >rx_av1.log 2>&1 &
rp=$!
sleep 0.3
"$SEND" send av1.ivf 0 "$p" >/dev/null
wait $rp || true
if [[ -s rx_av1.ivf ]]; then vmd5 rx_av1.ivf >rx_av1.md5; check "C2 av1 (senca loopback, FFmpeg decode)" exact av1.src rx_av1.md5; else fail "C2 av1 (no output)"; fi

echo "== A/D. mediamtx"
MTX=$(alloc)
cat >mediamtx.yml <<EOF
logLevel: warn
rtsp: true
rtspTransports: [udp, tcp]
rtspAddress: :$MTX
rtpAddress: :$((MTX + 2))
rtcpAddress: :$((MTX + 3))
rtspAuthMethods: [digest]
rtmp: false
hls: false
webrtc: false
srt: false
api: false
authInternalUsers:
  - user: any
    pass:
    permissions:
      - action: publish
      - action: read
        path: "~^open"
  - user: viewer
    pass: s3cret-pass
    permissions:
      - action: read
paths:
  all_others:
EOF
CID=$(docker run --rm -d --network host -v "$WORK/mediamtx.yml:/mediamtx.yml:ro" "$IMAGE")
CONTAINERS+=("$CID")
for _ in $(seq 100); do
    (exec 3<>/dev/tcp/127.0.0.1/$MTX) 2>/dev/null && break
    sleep 0.1
done
URL=rtsp://127.0.0.1:$MTX
push() { # file path
    "${FF[@]}" -re -i "$1" -map 0 -c copy -rtsp_transport tcp -f rtsp "$URL/$2" >"push_$2.log" 2>&1 &
    PIDS+=($!)
}
push av264.mkv open264
push av265.mkv open265
push av264.mkv secret
sleep 2.5
pull() { # name path transport extra...
    local name=$1 path=$2 tr=$3
    shift 3
    timeout 40 "$PULL" "$URL/$path" "$tr" "pull_$name" 5 "$@" >"pull_$name.log" 2>&1
}
pull 264u open264 udp &
P1=$!
pull 264t open264 tcp &
P2=$!
pull 265u open265 udp &
P3=$!
pull 265t open265 tcp &
P4=$!
pull secret secret tcp --user viewer --pass s3cret-pass &
P5=$!
r_nocred=0
timeout 20 "$PULL" "$URL/secret" tcp pull_nocred 2 >pull_nocred.log 2>&1 || r_nocred=$?
r_wrong=0
timeout 20 "$PULL" "$URL/secret" tcp pull_wrong 2 --user viewer --pass wrong >pull_wrong.log 2>&1 || r_wrong=$?
for p in $P1 $P2 $P3 $P4 $P5; do wait "$p" || true; done
# 5 秒の pull で、映像は 90 フレーム以上（キーフレームから）、音声は 150 パケット以上が元の連続した一部と一致
a_check() { # name codec-name
    local name=$1 log="pull_$1.log"
    local v a
    v=$(awk '$3 ~ /h26/ {print $4}' "$log")
    a=$(awk '$3 ~ /aac|opus/ {print $4}' "$log")
    local src=${2}
    if [[ -z "$v" || -z "$a" ]]; then fail "A $name (pull failed: $(tail -1 "$log"))"; return; fi
    vmd5 "$v" >"pull_$name.v.md5"
    check "A $name video" run "$src.v.src" "pull_$name.v.md5" 90
    check "A $name audio" run "$src.a.src" "$a" 150
}
a_check 264u av264
a_check 264t av264
a_check 265u av265
a_check 265t av265
a_check secret av264
if grep -q "TRACK" pull_secret.log; then pass "D digest auth accepted (MD5)"; else fail "D digest auth accepted"; fi
if [[ $r_nocred -ne 0 ]] && grep -q "401" pull_nocred.log; then pass "D no credentials rejected"; else fail "D no credentials rejected (rc=$r_nocred)"; fi
if [[ $r_wrong -ne 0 ]] && grep -q "401" pull_wrong.log; then pass "D wrong password rejected"; else fail "D wrong password rejected (rc=$r_wrong)"; fi
docker stop -t 2 "$CID" >/dev/null 2>&1 || true

echo "== B. senca rtsp_serve → FFmpeg"
b_case() { # name input src transport [user pass]
    local name=$1 in=$2 src=$3 tr=$4 user=${5:-} pw=${6:-}
    local p
    p=$(alloc)
    local auth=() cred=""
    if [[ -n "$user" ]]; then
        auth=(--user "$user" --pass "$pw")
        cred="$user:$pw@"
    fi
    "$SERVE" "$p" "$in" "${auth[@]}" >"serve_$name.out" 2>"serve_$name.err" &
    local sp=$!
    PIDS+=($sp)
    for _ in $(seq 100); do grep -q LISTENING "serve_$name.out" 2>/dev/null && break; sleep 0.05; done
    timeout 40 "${FF[@]}" -rtsp_transport "$tr" -i "rtsp://${cred}127.0.0.1:$p/stream" \
        -map 0:v -fps_mode passthrough -f framemd5 "b_$name.v" -map 0:a -c copy -f framemd5 "b_$name.a" || true
    wait $sp || true
    for k in v a; do grep -v '^#' "b_$name.$k" 2>/dev/null | awk -F, '{gsub(/ /,"",$6); print $6}' >"b_$name.$k.md5" || true; done
    check "B $name video" exact "$src.v.src" "b_$name.v.md5"
    check "B $name audio" exact "$src.a.src" "b_$name.a.md5"
}
b_case 264tcp short264.ts short264 tcp
b_case 264udp short264.ts short264 udp
b_case 265tcp short265.ts short265 tcp
b_case 265udp short265.ts short265 udp
b_case 264digest short264.ts short264 tcp alice pa55-word

echo "== $PASS passed, $FAIL failed"
[[ $FAIL -eq 0 ]]
