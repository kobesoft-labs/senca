#!/usr/bin/env bash
# senca-codec のソフトウェアコーデック（dav1d・libvpx・SVT-AV1・libopus）と senca-codec-worker の相互運用試験。
#
# 1. 復号: FFmpeg（libvpx・libvpx-vp9・libaom-av1）で符号化した IVF を senca-codec で復号し、
#    FFmpeg の独立な復号器（vp8・vp9 は FFmpeg 内蔵、AV1 は libaom）の出力と YUV の md5 が一致すること。
# 2. 符号化: senca-codec（libvpx・SVT-AV1）で符号化した IVF を ffprobe が全フレーム読めること、
#    FFmpeg の独立な復号器と senca-codec の復号器の出力が一致すること、元画像との PSNR が十分なこと。
# 3. Opus: senca-codec で符号化した Ogg Opus を FFmpeg（内蔵の opus 復号器）で復号でき、元の音と相関すること。
#    FFmpeg（libopus）で符号化した Ogg Opus を senca-codec で復号し、FFmpeg の復号と一致（SNR）すること。
# 4. codec worker: サンドボックスの自己試験（socket が拒否される）と、プール・故障注入（crash・hang・lie）の試験。
#
# ライブラリは SENCA_CODEC_LIB_DIR（既定は ~/tools/codeclibs/usr/lib/x86_64-linux-gnu。
# `apt-get download libdav1d7 libvpx12 libopus0 libsvtav1enc2` を `dpkg -x` したもの）から読み込む。
# FFmpeg は試験相手としてだけ使う。
set -euo pipefail

ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/../.." && pwd)"
CARGO="${CARGO:-$HOME/.cargo/bin/cargo}"
FFMPEG="${FFMPEG:-$HOME/tools/ffmpeg}"
FFPROBE="${FFPROBE:-$HOME/tools/ffprobe}"
PYTHON="${PYTHON:-python3}"
export CARGO_TARGET_DIR="${CARGO_TARGET_DIR:-$ROOT/target}"
export SENCA_CODEC_LIB_DIR="${SENCA_CODEC_LIB_DIR:-$HOME/tools/codeclibs/usr/lib/x86_64-linux-gnu}"

WORK="$(mktemp -d)"
cleanup() { rm -rf "$WORK"; }
trap cleanup EXIT

PASS=0
FAIL=0
pass() { echo "PASS: $1"; PASS=$((PASS + 1)); }
fail() { echo "FAIL: $1"; FAIL=$((FAIL + 1)); }

ff() { "$FFMPEG" -nostdin -hide_banner -v error -y "$@"; }
md5() { md5sum "$1" | cut -d' ' -f1; }

# 比較用の小さな Python（隔離モード、引数でファイルを渡す）。
cat >"$WORK/pcm_cmp.py" <<'EOF'
import sys, struct, math
# pcm_cmp.py <a.s16le> <b.s16le> <min_snr_db> [<offset_b>]
a = open(sys.argv[1], 'rb').read(); b = open(sys.argv[2], 'rb').read()
min_snr = float(sys.argv[3]); off = int(sys.argv[4]) if len(sys.argv) > 4 else 0
n = min(len(a), len(b) - 2 * off) // 2
if n < 4800:
    print(f"too short: {len(a)} {len(b)}"); sys.exit(1)
xa = struct.unpack(f'<{n}h', a[:2 * n]); xb = struct.unpack(f'<{n}h', b[2 * off:2 * off + 2 * n])
sig = sum(v * v for v in xa) or 1
err = sum((p - q) ** 2 for p, q in zip(xa, xb)) or 1
snr = 10 * math.log10(sig / err)
print(f"snr={snr:.1f}dB samples={n} len_a={len(a)//2} len_b={len(b)//2}")
sys.exit(0 if snr >= min_snr else 1)
EOF

echo "== build"
(cd "$ROOT" && "$CARGO" build -q -p senca-codec --example codec_tool && "$CARGO" build -q -p senca-codec-worker)
TOOL="$CARGO_TARGET_DIR/debug/examples/codec_tool"
WORKER="$CARGO_TARGET_DIR/debug/senca-codec-worker"

echo "== probe"
"$TOOL" probe | tee "$WORK/probe.txt"
for p in dav1d libvpx svt-av1 opus; do
    if grep -q "^provider $p available=true" "$WORK/probe.txt"; then pass "probe $p"; else fail "probe $p (library missing?)"; fi
done
# H.264/H.265 のソフトウェア能力は出ない（ADR-0004）。
if grep -E '^cap (dav1d|libvpx|svt-av1|opus) (h264|h265|aac)' "$WORK/probe.txt"; then
    fail "software provider advertises a patent-pooled codec"
else
    pass "no software H.264/H.265/AAC capability"
fi

W=176
H=144
FPS=25
N=30
ff -f lavfi -i "testsrc2=size=${W}x${H}:rate=${FPS}" -frames:v $N -pix_fmt yuv420p -f rawvideo "$WORK/src.yuv"
RAW=(-f rawvideo -pix_fmt yuv420p -s "${W}x${H}" -r "$FPS")

# FFmpeg の独立な復号器
ref_decoder() {
    case "$1" in
        vp8) echo vp8 ;;
        vp9) echo vp9 ;;
        av1) echo libaom-av1 ;;
    esac
}

echo "== 1. decode FFmpeg streams with senca-codec"
for codec in vp8 vp9 av1; do
    case "$codec" in
        vp8) enc=(-c:v libvpx -b:v 300k -auto-alt-ref 1 -lag-in-frames 16) ;;
        vp9) enc=(-c:v libvpx-vp9 -b:v 300k -auto-alt-ref 1 -lag-in-frames 16) ;;
        av1) enc=(-c:v libaom-av1 -cpu-used 8 -b:v 300k -g 12) ;;
    esac
    ivf="$WORK/ff_$codec.ivf"
    if ! ff "${RAW[@]}" -i "$WORK/src.yuv" "${enc[@]}" -f ivf "$ivf"; then
        fail "$codec: ffmpeg encode"
        continue
    fi
    ff -c:v "$(ref_decoder $codec)" -i "$ivf" -f rawvideo -pix_fmt yuv420p "$WORK/ref_$codec.yuv"
    if "$TOOL" decode "$codec" "$ivf" "$WORK/ours_$codec.yuv" 2>"$WORK/log"; then
        if [[ "$(md5 "$WORK/ref_$codec.yuv")" == "$(md5 "$WORK/ours_$codec.yuv")" ]]; then
            pass "$codec decode matches ffmpeg $(ref_decoder $codec) ($(tail -1 "$WORK/log"))"
        else
            fail "$codec decode differs from ffmpeg ($(stat -c%s "$WORK/ref_$codec.yuv") vs $(stat -c%s "$WORK/ours_$codec.yuv") bytes)"
        fi
    else
        fail "$codec decode: $(cat "$WORK/log")"
    fi
done

echo "== 2. encode with senca-codec, decode with FFmpeg"
for spec in vp8:ll vp9:ll vp9:ra av1:ll av1:ra; do
    codec="${spec%%:*}"
    mode="${spec##*:}"
    ivf="$WORK/ours_${codec}_$mode.ivf"
    if ! "$TOOL" encode "$codec" $W $H $FPS "$WORK/src.yuv" "$ivf" 400000 "$mode" 2>"$WORK/log"; then
        fail "$spec encode: $(tail -3 "$WORK/log")"
        continue
    fi
    n="$("$FFPROBE" -v error -count_frames -select_streams v:0 -show_entries stream=nb_read_frames -of csv=p=0 "$ivf" || echo 0)"
    if [[ "$n" == "$N" ]]; then pass "$spec: ffprobe reads $n frames ($(tail -1 "$WORK/log"))"; else fail "$spec: ffprobe read $n of $N frames"; fi
    if ! ff -xerror -c:v "$(ref_decoder $codec)" -i "$ivf" -f rawvideo -pix_fmt yuv420p "$WORK/ffdec.yuv"; then
        fail "$spec: ffmpeg $(ref_decoder $codec) failed to decode our stream"
        continue
    fi
    "$TOOL" decode "$codec" "$ivf" "$WORK/selfdec.yuv" 2>/dev/null
    if [[ "$(md5 "$WORK/ffdec.yuv")" == "$(md5 "$WORK/selfdec.yuv")" ]]; then
        pass "$spec: ffmpeg and senca decoders agree bit-exactly"
    else
        fail "$spec: ffmpeg and senca decoders disagree"
    fi
    psnr="$("$FFMPEG" -nostdin -hide_banner -v info "${RAW[@]}" -i "$WORK/ffdec.yuv" "${RAW[@]}" -i "$WORK/src.yuv" \
        -lavfi psnr -f null - 2>&1 | sed -n 's/.*PSNR.* average:\([0-9.inf]*\).*/\1/p' | tail -1)"
    if [[ -n "$psnr" ]] && awk -v p="$psnr" 'BEGIN { exit !(p == "inf" || p + 0 >= 28) }'; then
        pass "$spec: PSNR $psnr dB"
    else
        fail "$spec: PSNR '$psnr' too low"
    fi
done

echo "== 3. Opus"
ff -f lavfi -i "sine=frequency=440:duration=2:sample_rate=48000" -af "pan=stereo|c0=c0|c1=0.5*c0" -f s16le -ar 48000 -ac 2 "$WORK/src.s16"
if "$TOOL" opus-encode 48000 2 "$WORK/src.s16" "$WORK/ours.ogg" 96000 2>"$WORK/log"; then
    if ff -xerror -c:a opus -i "$WORK/ours.ogg" -f s16le -ar 48000 -ac 2 "$WORK/ffdec.s16"; then
        pass "opus: ffmpeg decodes our Ogg Opus ($(tail -1 "$WORK/log"))"
        if out="$("$PYTHON" -I "$WORK/pcm_cmp.py" "$WORK/src.s16" "$WORK/ffdec.s16" 10)"; then
            pass "opus: ffmpeg decode of our stream matches the source ($out)"
        else
            fail "opus: ffmpeg decode of our stream does not match the source ($out)"
        fi
        dur="$("$FFPROBE" -v error -show_entries format=duration -of csv=p=0 "$WORK/ours.ogg")"
        if awk -v d="$dur" 'BEGIN { exit !(d > 1.98 && d < 2.03) }'; then pass "opus: duration $dur s"; else fail "opus: duration $dur s"; fi
    else
        fail "opus: ffmpeg failed to decode our stream"
    fi
else
    fail "opus encode: $(cat "$WORK/log")"
fi
ff -f s16le -ar 48000 -ac 2 -i "$WORK/src.s16" -c:a libopus -b:a 96k "$WORK/ff.ogg"
ff -c:a libopus -i "$WORK/ff.ogg" -f s16le -ar 48000 -ac 2 "$WORK/ffref.s16"
if "$TOOL" opus-decode "$WORK/ff.ogg" "$WORK/ours.s16" 2>"$WORK/log"; then
    if out="$("$PYTHON" -I "$WORK/pcm_cmp.py" "$WORK/ffref.s16" "$WORK/ours.s16" 40)"; then
        pass "opus: decode of ffmpeg libopus stream matches ffmpeg ($out)"
    else
        fail "opus: decode differs from ffmpeg ($out)"
    fi
else
    fail "opus decode: $(cat "$WORK/log")"
fi

echo "== 4. codec worker"
if out="$("$WORKER" --self-test-sandbox 2>/dev/null)" && grep -q "socket_denied=1" <<<"$out"; then
    pass "worker sandbox self-test ($out)"
else
    fail "worker sandbox self-test (${out:-no output})"
fi
if (cd "$ROOT" && SENCA_CODEC_REQUIRE_SW=1 "$CARGO" test -q -p senca-codec --test software >"$WORK/log" 2>&1); then
    pass "senca-codec software round-trip tests"
else
    tail -20 "$WORK/log"
    fail "senca-codec software round-trip tests"
fi
if (cd "$ROOT" && "$CARGO" test -q -p senca-codec-worker --test pool >"$WORK/log" 2>&1); then
    pass "worker pool tests incl. fault injection crash/hang/lie/oversize ($(grep -o '[0-9]* passed' "$WORK/log" | tail -1))"
else
    tail -20 "$WORK/log"
    fail "worker pool / fault injection tests"
fi

echo
echo "passed: $PASS, failed: $FAIL"
[[ $FAIL -eq 0 ]]
